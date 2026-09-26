use super::*;
#[cfg(unix)]
use crate::test_log::CapturedLog;
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

/// A relay proxy that the SSH server turns away once, in the way `ssh`
/// reports it, and then answers hello.
#[cfg(unix)]
fn relay_proxy_refused_once(directory: &std::path::Path, refusal: &str) -> CommandSpec {
    let counter = directory.join("attempts");
    let script = format!(
        r#"
count=$(cat {counter} 2>/dev/null || echo 0)
echo $((count + 1)) > {counter}
if [ "$count" -eq 0 ]; then
  printf '{refusal}' >&2
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
    CommandSpec::new("sh", ["-c".to_owned(), script])
        .ssh_destination("build@10.0.0.1")
        .purpose("refused relay fixture")
}

/// Run the named test alone in a child process with a global subscriber, so
/// the log also has what other threads say, such as the thread that reaps a
/// dropped relay proxy. Returns the log in the child, and `None` in the
/// parent once the child has passed.
#[cfg(unix)]
fn global_log_in_isolated_child(test: &str) -> Option<CapturedLog> {
    const CHILD: &str = "MJ_WORKER_CLIENT_GLOBAL_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().expect("temp dir");
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(module_path!(), test),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return None;
    }
    let log = CapturedLog::default();
    tracing::subscriber::set_global_default(log.clone()).expect("the only global subscriber");
    Some(log)
}

/// Connect on a runtime of this test's own, for a test that runs with a
/// global subscriber, then give the threads that reap dropped proxies time
/// to report.
#[cfg(unix)]
fn connect_and_let_reapers_report(spec: &CommandSpec) -> Result<RelayClient> {
    let connected = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(RelayClient::connect_with_timeout(
            spec,
            SESSION_ID,
            Duration::from_secs(10),
        ));
    std::thread::sleep(Duration::from_millis(300));
    connected
}

/// R7-1: a shared SSH connection at `MaxSessions` refuses one more session
/// many times while sessions start, and the retry gets in. Neither the retry,
/// the proxy's own stderr, nor the reaping of the refused proxy is a warning
/// then; the refusal is logged once, at debug level, by the shared refusal
/// routine.
#[cfg(unix)]
#[test]
fn a_relay_proxy_refused_by_max_sessions_retries_without_a_warning() {
    let Some(log) = global_log_in_isolated_child(
        "a_relay_proxy_refused_by_max_sessions_retries_without_a_warning",
    ) else {
        return;
    };
    mj_core::targets::set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
    let directory = tempfile::tempdir().expect("temp dir");
    let spec = relay_proxy_refused_once(
        directory.path(),
        r"mux_client_request_session: session request failed: Session open refused by peer\nConnection closed by UNKNOWN port 65535\n",
    );
    connect_and_let_reapers_report(&spec).expect("the refused session is retried");

    let warnings = log.at_or_above(tracing::Level::WARN);
    assert!(warnings.is_empty(), "no warning expected: {warnings:#?}");
    let retries = log
        .at(tracing::Level::DEBUG)
        .into_iter()
        .filter(|text| text.contains("(MaxSessions); retrying"))
        .collect::<Vec<_>>();
    assert_eq!(retries.len(), 1, "{:#?}", log.events());
    assert!(
        retries[0].contains("Session open refused by peer"),
        "the retry keeps what ssh said: {}",
        retries[0]
    );
}

/// A connection dropped before authentication can mean a master died, so its
/// retry stays a warning, and it still names what ssh said.
#[cfg(unix)]
#[tokio::test]
async fn a_relay_proxy_dropped_before_authentication_still_warns() {
    mj_core::targets::set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
    let directory = tempfile::tempdir().expect("temp dir");
    let spec = relay_proxy_refused_once(
        directory.path(),
        r"kex_exchange_identification: read: Connection reset by peer\n",
    );
    let log = CapturedLog::default();
    let connected = {
        let _default = tracing::subscriber::set_default(log.clone());
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10)).await
    };
    mj_core::targets::set_ssh_retry_backoff_for_test(None);
    connected.expect("the dropped connection is retried");

    let warnings = log.at_or_above(tracing::Level::WARN);
    assert_eq!(warnings.len(), 1, "{warnings:#?}");
    assert!(
        warnings[0].contains("before authentication; retrying")
            && warnings[0].contains("kex_exchange_identification"),
        "{}",
        warnings[0]
    );
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

    let log = CapturedLog::default();
    let connected = {
        let _default = tracing::subscriber::set_default(log.clone());
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10)).await
    };
    let Err(error) = connected else {
        panic!("a proxy that exits 1 is a real failure");
    };
    // Not a refusal, so it is a warning, and the proxy's own complaint is in it.
    let warnings = log.at_or_above(tracing::Level::WARN);
    assert_eq!(warnings.len(), 1, "{warnings:#?}");
    assert!(
        warnings[0].contains("worker socket path is too long")
            && warnings[0].contains("relay proxy disconnected during hello"),
        "{}",
        warnings[0]
    );

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

/// What the worker's relay proxy prints, after its own log line, when the
/// worker has not bound its control socket yet.
#[cfg(unix)]
const WORKER_SOCKET_MISSING: &str = r"Error: connect worker socket /w/control.sock\n\nCaused by:\n    0: connect unix socket /w/control.sock\n    1: No such file or directory (os error 2)\n";

/// A local relay proxy for a worker that binds its control socket only after
/// `misses` connection attempts, or never when `misses` is `None`.
#[cfg(unix)]
fn relay_proxy_before_the_worker_binds(
    directory: &std::path::Path,
    misses: Option<u32>,
) -> CommandSpec {
    let counter = directory.join("attempts");
    let script = format!(
        r#"
count=$(cat {counter} 2>/dev/null || echo 0)
echo $((count + 1)) > {counter}
if [ "$count" -lt {misses} ]; then
  printf '{WORKER_SOCKET_MISSING}' >&2
  exit 1
fi
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"request_id":"%s","protocol_version":1,"result":"ok","payload":{{"type":"hello","data":{{"negotiated":1,"relay_version":"retry-fixture","session_id":"{session}"}}}}}}\n' "$id"
while IFS= read -r request; do :; done
"#,
        counter = counter.display(),
        misses = misses.unwrap_or(u32::MAX),
        session = SESSION_ID
    );
    CommandSpec::new("sh", ["-c".to_owned(), script]).purpose("starting worker relay fixture")
}

#[cfg(unix)]
fn relay_proxy_attempts(directory: &std::path::Path) -> u32 {
    std::fs::read_to_string(directory.join("attempts"))
        .expect("the fixture records its attempts")
        .trim()
        .parse()
        .unwrap()
}

/// R9-2: the daemon connects to a local worker about 34 ms after starting
/// it, before the worker has bound its control socket. Every start logged
/// two warnings for that ("relay request failed ... No such file or
/// directory" and "dropped relay proxy exited unsuccessfully") though every
/// session started normally. A socket that appears within the retry budget
/// is a routine retry, logged at debug level, and the exit of each proxy that
/// found no socket is not reported again when that proxy is reaped.
#[cfg(unix)]
#[test]
fn a_worker_socket_that_appears_within_the_retry_budget_is_not_a_warning() {
    let Some(log) = global_log_in_isolated_child(
        "a_worker_socket_that_appears_within_the_retry_budget_is_not_a_warning",
    ) else {
        return;
    };
    let directory = tempfile::tempdir().expect("temp dir");
    let spec = relay_proxy_before_the_worker_binds(directory.path(), Some(2));

    let client = connect_and_let_reapers_report(&spec)
        .expect("the connection is retried until the worker binds its socket");
    assert_eq!(client.relay_version(), "retry-fixture");
    assert_eq!(relay_proxy_attempts(directory.path()), 3);

    let warnings = log.at_or_above(tracing::Level::WARN);
    assert!(warnings.is_empty(), "no warning expected: {warnings:#?}");
    let retries = log
        .at(tracing::Level::DEBUG)
        .into_iter()
        .filter(|text| text.contains("has not bound its control socket yet"))
        .count();
    assert_eq!(retries, 2, "{:#?}", log.events());
}

/// A socket still missing after the retry budget is a real failure: one
/// warning that carries the proxy's complaint, and the same dead-transport
/// error that worker recovery reads.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_socket_still_missing_after_the_retry_budget_is_one_warning() {
    let directory = tempfile::tempdir().expect("temp dir");
    let spec = relay_proxy_before_the_worker_binds(directory.path(), None);
    let log = CapturedLog::default();
    let connected = {
        let _default = tracing::subscriber::set_default(log.clone());
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10)).await
    };
    let Err(error) = connected else {
        panic!("a worker that never binds its socket cannot be reached");
    };
    assert!(
        RelayTransportDead::marks_failed_handshake(&error),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("No such file or directory"),
        "{error:#}"
    );
    assert!(
        relay_proxy_attempts(directory.path()) > 1,
        "a missing socket is retried before it is reported"
    );

    let warnings = log.at_or_above(tracing::Level::WARN);
    assert_eq!(warnings.len(), 1, "{warnings:#?}");
    assert!(
        warnings[0].contains("control socket is still missing")
            && warnings[0].contains("No such file or directory"),
        "{}",
        warnings[0]
    );
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

fn skills_sync_target(profile_home: &std::path::Path) -> CredentialSyncTarget {
    CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Claude,
        profile_home: profile_home.to_path_buf(),
        authenticates_with_api_key: false,
        sync_github_token: false,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    }
}

/// Every session runs from a staged home of its own, so every session is
/// pushed the managed skills along with the profile's own.
#[test]
fn every_session_is_pushed_the_managed_skills_too() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review").unwrap();

    let target = skills_sync_target(home.path());
    for format in [
        mj_core::skills::SkillsArchiveFormat::Plain,
        mj_core::skills::SkillsArchiveFormat::Gzip,
    ] {
        let archive = canonical_session_skills(&target, format).unwrap();
        assert_eq!(
            archive,
            mj_core::skills::session_skills(target.harness, home.path(), format).unwrap()
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
}

/// Plays a worker for one skills push: it reports a stale tree, then installs
/// whatever archive it is sent, keeps a copy at the path in its third
/// argument, and reports the fingerprint of the archive's uncompressed
/// content, as a worker does.
#[cfg(unix)]
const SKILLS_FORMAT_RELAY: &str = r#"
import base64, gzip, hashlib, json, sys
protocol, session, received = int(sys.argv[1]), sys.argv[2], sys.argv[3]
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "skills-format-fixture", "session_id": session}}
    elif method == "skills_state":
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": "stale"}}
    elif method == "install_skills":
        archive = base64.b64decode(req["request"]["params"]["data"])
        with open(received, "wb") as out:
            out.write(archive)
        body = archive[8:]
        if archive[:8] == b"HELSKIL2":
            body = gzip.decompress(body)
        fingerprint = hashlib.sha256(b"HELSKIL1" + body).hexdigest()
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": fingerprint}}
    else:
        raise AssertionError(method)
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;

/// A worker from before relay protocol 23 reads only the uncompressed
/// `HELSKIL1` archive, whose limits count raw bytes. The controller sends it
/// that format and leaves out the 2.3 MB page it could not take; a current
/// worker gets a compressed archive with the page. Either way the push
/// succeeds only if the worker's fingerprint of what it received matches the
/// controller's.
#[cfg(unix)]
#[tokio::test]
async fn skills_are_pushed_in_the_archive_format_the_worker_reads() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/viz/demos")).unwrap();
    std::fs::write(home.path().join("skills/viz/SKILL.md"), "viz").unwrap();
    std::fs::write(
        home.path().join("skills/viz/demos/sunspot-pretty.html"),
        "<tr><td>1749-01</td><td>96.7</td></tr>\n".repeat(60_000),
    )
    .unwrap();

    for (protocol, magic, carries_page) in [
        (22, b"HELSKIL1", false),
        (RELAY_PROTOCOL_VERSION, b"HELSKIL2", true),
    ] {
        let scratch = tempfile::tempdir().unwrap();
        let received = scratch.path().join("received");
        let mut target = skills_sync_target(home.path());
        target.authenticates_with_api_key = true;
        target.spec = CommandSpec::new(
            "python3",
            [
                "-c".to_owned(),
                SKILLS_FORMAT_RELAY.to_owned(),
                protocol.to_string(),
                SESSION_ID.to_owned(),
                received.to_string_lossy().into_owned(),
            ],
        )
        .purpose("skills archive format fixture");

        let actions = reconcile_session(&target, None).await.unwrap();

        assert_eq!(
            actions,
            [CredentialSyncAction::SkillsPushed],
            "protocol {protocol}"
        );
        let archive = std::fs::read(&received).unwrap();
        assert!(archive.starts_with(magic), "protocol {protocol}");
        let sent = mj_core::skills::SkillsArchive::decode(&archive).unwrap();
        assert!(
            sent.entries()
                .iter()
                .any(|entry| entry.path == "skills/viz/SKILL.md"),
            "protocol {protocol}"
        );
        assert_eq!(
            sent.entries()
                .iter()
                .any(|entry| entry.path == "skills/viz/demos/sunspot-pretty.html"),
            carries_page,
            "protocol {protocol}"
        );
    }
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

/// Plays a worker for a credential sync: it reports the credential copy and
/// the skills fingerprint it is given, and keeps any credential file it is
/// sent at the path in its last argument.
#[cfg(unix)]
const CREDENTIAL_RELAY: &str = r#"
import base64, hashlib, json, sys
protocol, session, fingerprint, freshness, skills, received = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "credential-fixture", "session_id": session}}
    elif method == "credential_state":
        payload = {"type": "credential_state", "data": {"present": True, "fingerprint": fingerprint, "freshness_epoch_ms": freshness}}
    elif method == "skills_state":
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": skills}}
    elif method == "install_credentials":
        data = base64.b64decode(req["request"]["params"]["data"])
        with open(received, "wb") as out:
            out.write(data)
        payload = {"type": "credential_state", "data": {"present": True, "fingerprint": hashlib.sha256(data).hexdigest(), "freshness_epoch_ms": None}}
    else:
        break
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;

/// A ChatGPT login as Codex writes it, refreshed at `refreshed`.
#[cfg(unix)]
fn codex_login(refreshed: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {"access_token": format!("access-{refreshed}")},
        "last_refresh": refreshed,
    }))
    .unwrap()
}

/// A sync target for one session of the Codex profile in `home`, whose worker
/// is [`CREDENTIAL_RELAY`] holding a login refreshed at `session_refreshed`.
/// A file the sync pushes lands at `received`.
#[cfg(unix)]
fn codex_sync_target(
    home: &std::path::Path,
    session_id: &str,
    session_refreshed: &str,
    received: &std::path::Path,
) -> CredentialSyncTarget {
    let mut target = CredentialSyncTarget {
        session_id: session_id.into(),
        profile_id: "codex4".into(),
        harness: mj_core::config::HarnessKind::Codex,
        profile_home: home.to_path_buf(),
        authenticates_with_api_key: false,
        sync_github_token: false,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    };
    let skills = canonical_session_skills(&target, mj_core::skills::SkillsArchiveFormat::Gzip)
        .unwrap()
        .state()
        .fingerprint;
    let session = CredentialSnapshot::of(
        mj_core::config::HarnessKind::Codex,
        &codex_login(session_refreshed),
    );
    target.spec = CommandSpec::new(
        "python3",
        [
            "-c".to_owned(),
            CREDENTIAL_RELAY.to_owned(),
            RELAY_PROTOCOL_VERSION.to_string(),
            session_id.to_owned(),
            session.fingerprint,
            session.freshness_epoch_ms.unwrap_or_default().to_string(),
            skills,
            received.to_string_lossy().into_owned(),
        ],
    )
    .purpose("credential sync fixture");
    target
}

/// #1160: a sync that a session's auth failure asked for says whether it
/// reached that session, even when the session's login already matched the
/// profile's. That match is what shows the profile's own login is the one the
/// provider refused, so a spawn on the profile can be refused at once. A
/// periodic sync still leaves sessions that agreed out of its outcomes.
#[cfg(unix)]
#[tokio::test]
async fn a_triggered_sync_reports_the_session_it_reached_with_nothing_to_change() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let received = scratch.path().join("received");
    std::fs::write(
        home.path().join("auth.json"),
        codex_login("2026-09-25T20:00:00.000Z"),
    )
    .unwrap();
    let target = codex_sync_target(
        home.path(),
        SESSION_ID,
        "2026-09-25T20:00:00.000Z",
        &received,
    );

    let triggered = reconcile_profile(std::slice::from_ref(&target), Some(SESSION_ID)).await;
    assert_eq!(
        triggered,
        [CredentialSyncOutcome {
            session_id: SESSION_ID.into(),
            outcome: Ok(Vec::new()),
        }]
    );
    assert!(!received.exists(), "nothing was pushed");

    let periodic = reconcile_profile(std::slice::from_ref(&target), None).await;
    assert!(periodic.is_empty(), "{periodic:?}");
}

/// #1160: after `mj login` rewrote codex4's login, it was not clear that live
/// sessions got it. The next periodic sync pushes the new file to every live
/// session of the profile that still holds the old one, a sub-agent child as
/// much as any other, without waiting for another failure.
#[cfg(unix)]
#[tokio::test]
async fn a_new_login_is_pushed_to_every_live_session_of_its_profile() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let before = "2026-09-25T20:00:00.000Z";
    let sessions = ["child-in-parent-container", "sibling"];
    let targets = sessions
        .iter()
        .map(|session| {
            codex_sync_target(home.path(), session, before, &scratch.path().join(session))
        })
        .collect::<Vec<_>>();
    // `mj login` at 22:49:05Z.
    let login = codex_login("2026-09-25T22:49:05.000Z");
    std::fs::write(home.path().join("auth.json"), &login).unwrap();

    let outcomes = reconcile_profile(&targets, None).await;

    assert_eq!(
        outcomes,
        sessions
            .iter()
            .map(|session| CredentialSyncOutcome {
                session_id: (*session).into(),
                outcome: Ok(vec![CredentialSyncAction::Pushed]),
            })
            .collect::<Vec<_>>()
    );
    for session in sessions {
        assert_eq!(
            std::fs::read(scratch.path().join(session)).unwrap(),
            login,
            "{session}"
        );
    }
}

#[tokio::test]
async fn credential_sync_preempted_by_lifecycle_does_not_report_a_login_result() {
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let _reservation = gate.reserve(SESSION_ID);
    let profile = tempfile::tempdir().unwrap();
    let target = CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Codex,
        profile_home: profile.path().to_path_buf(),
        authenticates_with_api_key: false,
        sync_github_token: false,
        spec: CommandSpec::new("must-not-start-a-proxy", Vec::<String>::new()),
    };
    let result =
        credential_sync::reconcile_profile_guarded(&[target], Some(SESSION_ID), Some(&gate)).await;
    assert!(
        result.is_empty(),
        "deferral must not report an unchanged or failed login"
    );
}
