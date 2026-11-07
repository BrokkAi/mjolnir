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

/// A host at its `MaxStartups` ceiling drops the surplus connection before
/// authentication. That says nothing about the worker, so connect retries
/// instead of reporting a dead relay and triggering worker recovery.
// Hard-won: e6ed54ed: sshd refusals during startup were reported as dead workers.
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
// Hard-won: 5c9afffd: successful MaxSessions retries produced four warning logs per session.
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
# Read the hello before exiting so the fixture tests a lost response,
# rather than racing the client's write and sometimes causing a broken pipe.
IFS= read -r hello
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

/// The shell script of a relay proxy for a worker that binds its control
/// socket only after `misses` connection attempts, or never when `misses` is
/// `None`.
#[cfg(unix)]
fn worker_binds_after_script(directory: &std::path::Path, misses: Option<u32>) -> String {
    let counter = directory.join("attempts");
    format!(
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
    )
}

/// A local relay proxy for a worker that binds its control socket only after
/// `misses` connection attempts, or never when `misses` is `None`.
#[cfg(unix)]
fn relay_proxy_before_the_worker_binds(
    directory: &std::path::Path,
    misses: Option<u32>,
) -> CommandSpec {
    CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            worker_binds_after_script(directory, misses),
        ],
    )
    .purpose("starting worker relay fixture")
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
// Hard-won: b52a038c: normal worker starts logged two warnings before the socket appeared.
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

/// The worker has not bound its socket yet: the SSH session ran, so its shard
/// accepted it, and only a refusal advances the walk. Each retry runs on the
/// same shard (the same control socket) instead of visiting, and maybe
/// opening, another master. The fixture is a fake `ssh` on the proxy's own
/// `PATH`, and a probe lease so no master is checked or opened.
// Hard-won: ca6580092: a retry after a missing worker socket reused a walk that had marked its shard visited, so each retry moved to another shard.
#[cfg(unix)]
#[test]
fn a_missing_worker_socket_retries_on_the_same_shard() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "MJ_WORKER_SOCKET_SHARD_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().expect("temp dir");
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(
                module_path!(),
                "a_missing_worker_socket_retries_on_the_same_shard",
            ),
        )
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let sockets = tempfile::tempdir_in("/tmp").expect("short control socket directory");
    mj_core::targets::set_ssh_connection_sharing_for_test(Some(
        mj_core::targets::SshSharingForTest::Directory(sockets.path().to_path_buf()),
    ));
    let directory = tempfile::tempdir().expect("temp dir");
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let used = directory.path().join("control-paths");
    let fake_ssh = bin.join("ssh");
    std::fs::write(
        &fake_ssh,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do case \"$arg\" in ControlPath=*) echo \"$arg\" >> {used};; esac; done\n{body}",
            used = used.display(),
            body = worker_binds_after_script(directory.path(), Some(4)),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let target = mj_core::targets::SshTarget {
        destination: "socket-shard-host".into(),
        ssh_args: Vec::new(),
    };
    let mut spec = CommandSpec::new("ssh", [target.destination.clone(), "worker-proxy".into()])
        .ssh_probe_session(&target)
        .purpose("starting worker relay fixture");
    spec.env.insert("PATH".into(), path);

    let client = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(RelayClient::connect_with_timeout(
            &spec,
            SESSION_ID,
            Duration::from_secs(10),
        ))
        .expect("the connection is retried until the worker binds its socket");
    mj_core::targets::set_ssh_connection_sharing_for_test(None);

    assert_eq!(client.relay_version(), "retry-fixture");
    assert_eq!(relay_proxy_attempts(directory.path()), 5);
    let used = std::fs::read_to_string(&used).unwrap();
    let distinct: std::collections::BTreeSet<_> = used.lines().collect();
    assert_eq!(
        (used.lines().count(), distinct.len()),
        (5, 1),
        "every attempt used one control socket: {used}"
    );
}

// Hard-won: 0a278358: a new controller could not handshake with v1 workers after protocol 2 pinned the range.
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
// Hard-won: d1744be0: long worker socket paths failed, but their stderr was missing from the launch error.
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

#[cfg(unix)]
#[tokio::test]
async fn credential_sync_pushes_a_refreshed_github_token_to_a_running_worker() {
    let script = r#"
import base64, hashlib, json, sys
session = sys.argv[1]
for line in sys.stdin:
    request = json.loads(line)
    method = request["request"]["method"]
    if method == "hello":
        payload = {"type": "hello", "data": {
            "negotiated": 1, "relay_version": "github-token-test", "session_id": session
        }}
    elif method == "github_token_state":
        payload = {"type": "github_token_state", "data": {"present": False, "fingerprint": ""}}
    elif method == "install_github_token":
        token = base64.b64decode(request["request"]["params"]["data"])
        payload = {"type": "github_token_state", "data": {
            "present": True, "fingerprint": hashlib.sha256(token).hexdigest()
        }}
    else:
        raise AssertionError(method)
    print(json.dumps({
        "request_id": request["request_id"], "protocol_version": 1,
        "result": "ok", "payload": payload
    }), flush=True)
"#;
    let spec = CommandSpec::new(
        "python3",
        [
            "-u".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            SESSION_ID.to_owned(),
        ],
    )
    .purpose("GitHub token sync test relay");
    let mut relay = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10))
        .await
        .expect("test worker relay connects");
    let source = tempfile::tempdir().unwrap();
    let token_path = source.path().join("agent-pat");
    let target = CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "codex".into(),
        harness: mj_core::config::HarnessKind::Codex,
        profile_home: std::path::PathBuf::new(),
        aws_credentials: None,
        skips_login_file_sync: true,
        sync_github_token: true,
        github: mj_core::config::GithubConfig {
            session_token_path: Some(token_path.clone()),
            ..Default::default()
        },
        skills_scope: mj_core::skills::SkillsScope::Isolated,
        spec,
    };

    for token in ["initial-session-pat", "rotated-session-pat"] {
        std::fs::write(&token_path, token).unwrap();
        let selected = crate::controller::github_token_for_session(
            target.session_id.clone(),
            target.github.clone(),
        )
        .await
        .unwrap();
        assert_eq!(selected.as_deref(), Some(token));
        let action =
            credential_sync::reconcile_github_token(&mut relay, &target, selected.as_deref())
                .await
                .unwrap();
        assert_eq!(action, Some(CredentialSyncAction::GithubTokenPushed));
    }
}

/// Plays a worker that speaks the protocol in its first argument and keeps
/// the slopcop login it is sent at the path in its third. Any request it does
/// not expect fails the exchange.
#[cfg(unix)]
const SLOPCOP_LOGIN_RELAY: &str = r#"
import base64, hashlib, json, os, sys
protocol, session, stored = int(sys.argv[1]), sys.argv[2], sys.argv[3]
def state():
    if not os.path.exists(stored):
        return {"present": False, "fingerprint": "", "freshness_epoch_ms": None}
    with open(stored, "rb") as copy:
        return {"present": True, "fingerprint": hashlib.sha256(copy.read()).hexdigest(), "freshness_epoch_ms": None}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "slopcop-login-fixture", "session_id": session}}
    elif method == "slopcop_login_state":
        payload = {"type": "credential_state", "data": state()}
    elif method == "install_slopcop_login":
        with open(stored, "wb") as copy:
            copy.write(base64.b64decode(req["request"]["params"]["data"]))
        payload = {"type": "credential_state", "data": state()}
    elif method == "remove_slopcop_login":
        os.remove(stored)
        payload = {"type": "credential_state", "data": state()}
    else:
        raise AssertionError(method)
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;

#[cfg(unix)]
#[tokio::test]
async fn slopcop_login_sync_converges_current_workers_and_never_asks_older_ones() {
    use credential_sync::HostSlopcopLogin::{SignedIn, SignedOut, Unknown};
    use mj_core::skills::SkillsScope;
    use mj_core::slopcop::SlopcopLogin;

    let scratch = tempfile::tempdir().unwrap();
    let stored = scratch.path().join("slopcop-login");
    let connect = |protocol: u32| {
        let spec = CommandSpec::new(
            "python3",
            [
                "-c".to_owned(),
                SLOPCOP_LOGIN_RELAY.to_owned(),
                protocol.to_string(),
                SESSION_ID.to_owned(),
                stored.to_string_lossy().into_owned(),
            ],
        )
        .purpose("slopcop login sync test relay");
        let mut target = skills_sync_target(scratch.path());
        target.skills_scope = SkillsScope::Isolated;
        target.spec = spec.clone();
        async move {
            let client =
                RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10))
                    .await
                    .expect("test worker relay connects");
            (client, target)
        }
    };
    let login = SlopcopLogin::new("first-token", Some("https://slopcop.example")).unwrap();
    let rotated = SlopcopLogin::new("rotated-token", None).unwrap();

    // A worker from before the login requests cannot decode them, so it is
    // never sent one; it gets the login once the current worker replaces it.
    let (mut client, target) = connect(mj_core::relay::RELAY_SLOPCOP_LOGIN_PROTOCOL - 1).await;
    let before = client.next_request;
    let action =
        credential_sync::reconcile_slopcop_login(&mut client, &target, &SignedIn(login.clone()))
            .await
            .unwrap();
    assert_eq!(action, None);
    assert_eq!(client.next_request, before);

    let (mut client, target) = connect(RELAY_PROTOCOL_VERSION).await;
    for (host, expected, copy) in [
        (
            SignedIn(login.clone()),
            Some(CredentialSyncAction::SlopcopLoginPushed),
            Some(&login),
        ),
        (SignedIn(login.clone()), None, Some(&login)),
        (
            SignedIn(rotated.clone()),
            Some(CredentialSyncAction::SlopcopLoginPushed),
            Some(&rotated),
        ),
        // An unreadable host login is not a signed-out one.
        (Unknown, None, Some(&rotated)),
        (
            SignedOut,
            Some(CredentialSyncAction::SlopcopLoginRemoved),
            None,
        ),
        (SignedOut, None, None),
    ] {
        let action = credential_sync::reconcile_slopcop_login(&mut client, &target, &host)
            .await
            .unwrap();
        assert_eq!(action, expected, "{host:?}");
        assert_eq!(
            std::fs::read(&stored).ok(),
            copy.map(SlopcopLogin::encode),
            "{host:?}"
        );
    }
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
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
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
async fn publishing_changed_targets_reconciles_only_the_affected_profile() {
    let profile = tempfile::tempdir().unwrap();
    let target = |session: &str, profile_id: &str| CredentialSyncTarget {
        session_id: session.into(),
        profile_id: profile_id.into(),
        harness: mj_core::config::HarnessKind::Codex,
        profile_home: profile.path().to_path_buf(),
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    };
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    let work = target("work-session", "work");
    let other = target("other-session", "other");
    handle.set_targets(vec![work.clone(), other.clone()]);
    let mut initial = BTreeSet::new();
    for _ in 0..2 {
        initial.insert(
            tokio::time::timeout(Duration::from_secs(5), coordinator.result())
                .await
                .unwrap()
                .unwrap()
                .profile_id,
        );
    }
    assert_eq!(initial, BTreeSet::from(["other".into(), "work".into()]));

    let mut child = target("child-session", "work");
    for changed_spec in [false, true] {
        if changed_spec {
            child.spec = CommandSpec::new("sh", ["-c", "exit 2"]);
        }
        // Reverse the existing targets as well: ordering isn't a change.
        handle.set_targets(vec![other.clone(), child.clone(), work.clone()]);
        let changed = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.profile_id, "work");
        assert!(changed.trigger.is_none());
        assert_eq!(changed.outcomes.len(), 2);

        // This explicit pass is a completion barrier for the other profile.
        // Its per-profile serialization means any unintended automatic pass
        // must complete before this one, regardless of subprocess timing.
        handle.sync_profile_now(
            "other",
            Some(mj_core::credentials::CredentialSyncCause {
                session_id: other.session_id.clone(),
                reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            }),
        );
        let explicit = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(explicit.profile_id, "other");
        assert!(
            explicit.trigger.is_some(),
            "unrelated profile was reconciled"
        );
    }
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

#[cfg(unix)]
fn skills_sync_target(profile_home: &std::path::Path) -> CredentialSyncTarget {
    CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Claude,
        profile_home: profile_home.to_path_buf(),
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
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

#[cfg(unix)]
#[tokio::test]
async fn mixed_localhost_and_isolated_sessions_sync_different_skills_from_one_profile() {
    use mj_core::skills::{SkillsArchive, SkillsScope};
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review").unwrap();
    std::fs::create_dir_all(home.path().join("skills/mj")).unwrap();
    std::fs::write(home.path().join("skills/mj/SKILL.md"), "old mj").unwrap();

    for protocol in [22, RELAY_PROTOCOL_VERSION] {
        let scratch = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        // Put isolated first: cache construction must not decide scope for
        // the other sessions sharing this profile.
        for (index, scope) in [SkillsScope::Isolated, SkillsScope::Localhost]
            .into_iter()
            .enumerate()
        {
            let mut target = skills_sync_target(home.path());
            target.session_id = format!("{index:032x}");
            target.skills_scope = scope;
            target.skips_login_file_sync = true;
            target.spec = CommandSpec::new(
                "python3",
                [
                    "-c".to_owned(),
                    SKILLS_FORMAT_RELAY.to_owned(),
                    protocol.to_string(),
                    target.session_id.clone(),
                    scratch
                        .path()
                        .join(&target.session_id)
                        .to_string_lossy()
                        .into_owned(),
                ],
            );
            targets.push(target);
        }
        let outcomes = credential_sync::reconcile_profile(&targets, None).await;
        assert_eq!(outcomes.len(), targets.len());
        for outcome in outcomes {
            assert_eq!(
                outcome.outcome.unwrap(),
                vec![CredentialSyncAction::SkillsPushed]
            );
        }
        for target in &targets {
            let archive = SkillsArchive::decode(
                &std::fs::read(scratch.path().join(&target.session_id)).unwrap(),
            )
            .unwrap();
            assert_eq!(
                archive,
                canonical_session_skills(
                    target,
                    if protocol == 22 {
                        mj_core::skills::SkillsArchiveFormat::Plain
                    } else {
                        mj_core::skills::SkillsArchiveFormat::Gzip
                    }
                )
                .unwrap()
            );
            let stage = tempfile::tempdir().unwrap();
            // Sync removes an mj copy left behind by an earlier build.
            std::fs::create_dir_all(stage.path().join("skills/mj")).unwrap();
            std::fs::write(stage.path().join("skills/mj/SKILL.md"), "old mj").unwrap();
            mj_core::skills::install_skills(target.harness, stage.path(), &archive).unwrap();
            assert_eq!(
                stage.path().join("skills/mj/SKILL.md").exists(),
                target.skills_scope == SkillsScope::Localhost
            );
            assert_eq!(
                stage
                    .path()
                    .join("skills/mj/references/configuration.md")
                    .exists(),
                target.skills_scope == SkillsScope::Localhost
            );
            assert_eq!(
                std::fs::read(stage.path().join("skills/review/SKILL.md")).unwrap(),
                b"review"
            );
        }
    }
}

/// A worker from before relay protocol 23 reads only the uncompressed
/// `HELSKIL1` archive, whose limits count raw bytes. The controller sends it
/// that format and leaves out the 2.3 MB page it could not take; a current
/// worker gets a compressed archive with the page. Either way the push
/// succeeds only if the worker's fingerprint of what it received matches the
/// controller's.
// Hard-won: 29894ad9: protocol-22 workers rejected the compressed skills archive sent by newer controllers.
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
        target.skips_login_file_sync = true;
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

        let skills = Arc::new(CanonicalSkills::collect(&target));
        let actions = reconcile_session(
            &SessionRelays::Direct,
            &target,
            &skills,
            &credential_sync::GithubSync::Known(None),
            &credential_sync::AwsSync::Unmanaged,
            &credential_sync::HostSlopcopLogin::Unknown,
            credential_sync::JobHold::new(None, None, None, None),
        )
        .await
        .unwrap();

        assert_eq!(
            actions.map(|sync| sync.actions),
            Some(vec![CredentialSyncAction::SkillsPushed]),
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
import base64, hashlib, json, os, sys
protocol, session, fingerprint, freshness, skills, received = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
# Optional: the first connection ever made fails, to play a flaky worker.
fail_first = sys.argv[7] if len(sys.argv) > 7 else ""
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        if fail_first and not os.path.exists(fail_first):
            open(fail_first, "w").close()
            sys.exit(1)
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "credential-fixture", "session_id": session}}
    elif method == "credential_state":
        payload = {"type": "credential_state", "data": {"present": True, "fingerprint": fingerprint, "freshness_epoch_ms": freshness}}
    elif method == "skills_state":
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": skills}}
    elif method == "aws_credentials_state":
        import pathlib
        current = pathlib.Path(received)
        data = current.read_bytes() if current.exists() else b""
        payload = {"type": "credential_state", "data": {"present": bool(data), "fingerprint": hashlib.sha256(data).hexdigest() if data else "", "freshness_epoch_ms": None}}
    elif method == "aws_credentials_scope":
        scope_file = received + ".scope"
        scope = json.load(open(scope_file)) if os.path.exists(scope_file) else None
        payload = {"type": "aws_credentials_scope", "data": {"scope": scope}}
    elif method == "read_credentials":
        payload = {"type": "credentials", "data": {"data": base64.b64encode(open(received, "rb").read()).decode()}}
    elif method == "remove_aws_credentials":
        import pathlib
        pathlib.Path(received).unlink(missing_ok=True)
        pathlib.Path(received + ".scope").unlink(missing_ok=True)
        payload = {"type": "credential_state", "data": {"present": False, "fingerprint": "", "freshness_epoch_ms": None}}
    elif method in ("install_credentials", "install_aws_credentials"):
        data = base64.b64decode(req["request"]["params"]["data"])
        with open(received, "wb") as out:
            out.write(data)
        if method == "install_aws_credentials":
            scope = req["request"]["params"].get("scope")
            if scope:
                json.dump(scope, open(received + ".scope", "w"))
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
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
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

// Hard-won: #1385: AWS reconciliation skipped credentials, and local sessions can share the same profile.
#[cfg(unix)]
#[tokio::test]
async fn aws_credentials_refresh_only_the_isolated_sessions_of_a_shared_profile() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let aws = bin.join("aws");
    // Recorded AWS process-format exports; the normal executor drains this
    // helper's output while the coordinator and relay run as in production.
    std::fs::write(
        &aws,
        "#!/bin/sh\nexec /bin/cat \"$MJ_AWS_EXPORT_FIXTURE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source_file = scratch.path().join("export.json");
    let local_received = scratch.path().join("local");
    let remote_received = scratch.path().join("remote");
    let mut local = codex_sync_target(
        home.path(),
        "local",
        "2026-09-25T20:00:00Z",
        &local_received,
    );
    local.skips_login_file_sync = true;
    let mut remote = codex_sync_target(
        home.path(),
        "remote",
        "2026-09-25T20:00:00Z",
        &remote_received,
    );
    remote.skips_login_file_sync = true;
    remote.aws_credentials = Some(mj_core::aws_credentials::AwsCredentialSource {
        environment: BTreeMap::from([
            ("PATH".into(), bin.to_string_lossy().into_owned()),
            ("AWS_PROFILE".into(), "bedrock".into()),
            (
                "MJ_AWS_EXPORT_FIXTURE".into(),
                source_file.to_string_lossy().into_owned(),
            ),
        ]),
        role: None,
    });
    let targets = [local, remote];
    for generation in ["initial", "renewed"] {
        std::fs::write(
            &source_file,
            serde_json::to_vec(&serde_json::json!({
                "Version": 1, "AccessKeyId": format!("key-{generation}"),
                "SecretAccessKey": format!("secret-{generation}"),
                "SessionToken": "s".repeat(70_000), "Expiration": "2099-01-01T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap();
        let outcomes = reconcile_profile(&targets, None).await;
        assert_eq!(
            outcomes,
            [CredentialSyncOutcome {
                session_id: "remote".into(),
                outcome: Ok(vec![CredentialSyncAction::Pushed]),
                step_failures: Vec::new(),
            }]
        );
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&remote_received).unwrap()).unwrap();
        assert_eq!(received["AccessKeyId"], format!("key-{generation}"));
        assert!(!local_received.exists());
        assert!(reconcile_profile(&targets, None).await.is_empty());
    }
    // A failed AWS source reports only the isolated session; the local
    // session still completes its ordinary reconciliation.
    std::fs::remove_file(&source_file).unwrap();
    let outcomes = reconcile_profile(&targets, None).await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].session_id, "remote");
    assert!(!outcomes[0].step_failures.is_empty());
}

// A profile with a role gives each session credentials assumed for it alone;
// the host's unscoped export never reaches the session, not even when the
// role cannot be assumed.
#[cfg(unix)]
#[tokio::test]
async fn role_scoped_sessions_receive_assumed_credentials_and_never_the_unscoped_export() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let calls = scratch.path().join("calls");
    // A stand-in for the AWS CLI: `configure export-credentials` is the
    // unscoped export and must never run; `sts assume-role` prints the
    // recorded output or the recorded AccessDenied line.
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!(
            "#!/bin/sh\ncase \"$1\" in configure) echo unscoped export ran >&2; exit 99;; esac\n\
             echo x >> {calls}\n\
             if [ -f \"$MJ_AWS_ASSUME_FIXTURE\" ]; then exec /bin/cat \"$MJ_AWS_ASSUME_FIXTURE\"; fi\n\
             /bin/cat {denied} >&2; exit 254\n",
            calls = calls.display(),
            denied = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/aws-sts-assume-role-access-denied.txt")
                .display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The recorded output, expiring far in the future so only the cache's
    // half-lifetime rule could ask for another call (it does not here).
    let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/aws-sts-assume-role.json"
    ))
    .unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let fixture = scratch.path().join("assumed.json");
    std::fs::write(&fixture, serde_json::to_vec(&recorded).unwrap()).unwrap();

    let source = |fixture: &std::path::Path| mj_core::aws_credentials::AwsCredentialSource {
        environment: BTreeMap::from([
            ("PATH".into(), bin.to_string_lossy().into_owned()),
            ("AWS_PROFILE".into(), "bedrock".into()),
            (
                "MJ_AWS_ASSUME_FIXTURE".into(),
                fixture.to_string_lossy().into_owned(),
            ),
        ]),
        role: Some(mj_core::config::AwsRoleConfig {
            role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
            session_policy: None,
            session_policy_arns: Vec::new(),
            duration_seconds: None,
            source_identity: false,
        }),
    };
    let granted_received = scratch.path().join("granted");
    let denied_received = scratch.path().join("denied");
    let mut granted = codex_sync_target(
        home.path(),
        "role-granted",
        "2026-09-25T20:00:00Z",
        &granted_received,
    );
    granted.skips_login_file_sync = true;
    granted.aws_credentials = Some(source(&fixture));
    let mut denied = codex_sync_target(
        home.path(),
        "role-denied",
        "2026-09-25T20:00:00Z",
        &denied_received,
    );
    denied.skips_login_file_sync = true;
    denied.aws_credentials = Some(source(&scratch.path().join("absent.json")));
    let targets = [granted, denied];

    let outcomes = reconcile_profile(&targets, None).await;
    let granted_outcome = outcomes
        .iter()
        .find(|outcome| outcome.session_id == "role-granted")
        .unwrap();
    assert_eq!(
        granted_outcome.outcome,
        Ok(vec![CredentialSyncAction::Pushed])
    );
    let received: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&granted_received).unwrap()).unwrap();
    assert_eq!(received["AccessKeyId"], "ASIAEXAMPLEEXAMPLE00");
    assert_eq!(received["SessionToken"], "example-session-token");
    // The refused session reports AWS's error and receives nothing.
    let denied_outcome = outcomes
        .iter()
        .find(|outcome| outcome.session_id == "role-denied")
        .unwrap();
    let error = denied_outcome.failure_text().unwrap();
    assert!(
        error.contains("AccessDenied") && error.contains("remove [profiles."),
        "{error}"
    );
    assert!(!error.contains("unscoped export ran"), "{error}");
    assert!(!denied_received.exists());

    // The next pass finds the cached credentials still fresh and installed:
    // no new assume-role call and no push. The refused session retries.
    let calls_before = std::fs::read_to_string(&calls).unwrap().lines().count();
    let again = reconcile_profile(&targets, None).await;
    assert!(
        again
            .iter()
            .all(|outcome| outcome.session_id == "role-denied")
    );
    let calls_after = std::fs::read_to_string(&calls).unwrap().lines().count();
    assert_eq!(calls_after, calls_before + 1);
}

#[cfg(unix)]
#[tokio::test]
async fn role_scoped_mints_for_one_profile_run_together() {
    use std::os::unix::fs::PermissionsExt;

    const SESSIONS: usize = 4;
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let started = scratch.path().join("started");
    // Each stand-in call announces itself and answers only once every
    // session's call has started. Run one after another, the first call
    // waits out its own bound and fails; run together, all four succeed.
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!(
            "#!/bin/sh\necho x >> {started}\n\
             tries=0\n\
             while [ \"$(/usr/bin/wc -l < {started})\" -lt {SESSIONS} ]; do\n\
               tries=$((tries + 1)); [ $tries -gt 200 ] && exit 3; /bin/sleep 0.05\n\
             done\n\
             exec /bin/cat \"$MJ_AWS_ASSUME_FIXTURE\"\n",
            started = started.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/aws-sts-assume-role.json"
    ))
    .unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let fixture = scratch.path().join("assumed.json");
    std::fs::write(&fixture, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let targets = (0..SESSIONS)
        .map(|index| {
            let mut target = codex_sync_target(
                home.path(),
                &format!("concurrent-{index}"),
                "2026-09-25T20:00:00Z",
                &scratch.path().join(format!("received-{index}")),
            );
            target.skips_login_file_sync = true;
            target.aws_credentials = Some(mj_core::aws_credentials::AwsCredentialSource {
                environment: BTreeMap::from([
                    ("PATH".into(), bin.to_string_lossy().into_owned()),
                    ("AWS_PROFILE".into(), "bedrock".into()),
                    (
                        "MJ_AWS_ASSUME_FIXTURE".into(),
                        fixture.to_string_lossy().into_owned(),
                    ),
                ]),
                role: Some(mj_core::config::AwsRoleConfig {
                    role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
                    session_policy: None,
                    session_policy_arns: Vec::new(),
                    duration_seconds: None,
                    source_identity: false,
                }),
            });
            target
        })
        .collect::<Vec<_>>();

    let outcomes = reconcile_profile(&targets, None).await;

    assert_eq!(outcomes.len(), SESSIONS, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.outcome == Ok(vec![CredentialSyncAction::Pushed])),
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_fast_session_is_installed_while_a_slow_one_is_still_minting() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // The stand-in AWS CLI answers at once, except for a session that names a
    // file to wait for: its call finishes only after that file exists, and
    // fails after its own bound otherwise.
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        "#!/bin/sh\n\
         tries=0\n\
         while [ -n \"$MJ_AWS_WAIT_FOR\" ] && [ ! -e \"$MJ_AWS_WAIT_FOR\" ]; do\n\
           tries=$((tries + 1)); [ $tries -gt 200 ] && exit 3; /bin/sleep 0.05\n\
         done\n\
         exec /bin/cat \"$MJ_AWS_ASSUME_FIXTURE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/aws-sts-assume-role.json"
    ))
    .unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let fixture = scratch.path().join("assumed.json");
    std::fs::write(&fixture, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let fast_received = scratch.path().join("fast-received");
    let target = |session: &str, received: &std::path::Path, wait_for: Option<&std::path::Path>| {
        let mut target = codex_sync_target(home.path(), session, "2026-09-25T20:00:00Z", received);
        target.skips_login_file_sync = true;
        let mut environment = BTreeMap::from([
            ("PATH".into(), bin.to_string_lossy().into_owned()),
            ("AWS_PROFILE".into(), "bedrock".into()),
            (
                "MJ_AWS_ASSUME_FIXTURE".into(),
                fixture.to_string_lossy().into_owned(),
            ),
        ]);
        if let Some(path) = wait_for {
            environment.insert(
                "MJ_AWS_WAIT_FOR".into(),
                path.to_string_lossy().into_owned(),
            );
        }
        target.aws_credentials = Some(mj_core::aws_credentials::AwsCredentialSource {
            environment,
            role: Some(mj_core::config::AwsRoleConfig {
                role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
                session_policy: None,
                session_policy_arns: Vec::new(),
                duration_seconds: None,
                source_identity: false,
            }),
        });
        target
    };
    // The slow session is first in the list: a pass that waited for it, or
    // took sessions in list order, would never install the fast one first.
    let targets = [
        target(
            "slow-session",
            &scratch.path().join("slow-received"),
            Some(&fast_received),
        ),
        target("fast-session", &fast_received, None),
    ];

    let outcomes = reconcile_profile(&targets, None).await;

    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.outcome == Ok(vec![CredentialSyncAction::Pushed])),
        "{outcomes:?}"
    );
}

/// Plays a worker that keeps, in the directory named by its third argument,
/// the AWS credentials, GitHub token and skills it is sent, and starts with a
/// stale skills tree. Any request it does not expect fails the exchange.
#[cfg(unix)]
const AWS_SYNC_RELAY: &str = r#"
import base64, gzip, hashlib, json, os, sys, time
protocol, session, root = int(sys.argv[1]), sys.argv[2], sys.argv[3]
# A slow worker: its first AWS answer waits for this file, or fails the exchange.
wait_for = sys.argv[4] if len(sys.argv) > 4 else ""
# A slow login step: the first skills request announces itself and waits for this file.
login_hold = sys.argv[5] if len(sys.argv) > 5 else ""
def path(name): return os.path.join(root, name)
def hold():
    # The first AWS request of a slow worker announces itself and then waits.
    if not wait_for: return
    open(path("waiting"), "w").close()
    tries = 0
    while not os.path.exists(wait_for):
        tries += 1
        if tries > 200: sys.exit(1)
        time.sleep(0.05)
def state(name):
    # Provider files missing or different: the worker reports the credentials
    # absent although the secret file may still be there.
    if name == "aws" and os.path.exists(path("provider-files-broken")):
        return {"present": False, "fingerprint": "", "freshness_epoch_ms": None}
    if not os.path.exists(path(name)):
        return {"present": False, "fingerprint": "", "freshness_epoch_ms": None}
    data = open(path(name), "rb").read()
    return {"present": True, "fingerprint": hashlib.sha256(data).hexdigest(), "freshness_epoch_ms": None}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    params = req["request"].get("params", {})
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "aws-sync-fixture", "session_id": session}}
    elif method == "aws_credentials_state":
        hold()
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "install_aws_credentials":
        hold()
        if os.path.exists(path("install-fails")): sys.exit(1)
        secret = base64.b64decode(params["data"])
        open(path("aws"), "wb").write(secret)
        # The scope record sits beside the secret, bound to it by fingerprint.
        if params.get("scope"):
            json.dump({"scope": params["scope"], "secret_fingerprint": hashlib.sha256(secret).hexdigest()}, open(path("aws-scope.json"), "w"))
        elif os.path.exists(path("aws-scope.json")):
            os.remove(path("aws-scope.json"))
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "aws_credentials_scope":
        hold()
        scope = None
        if os.path.exists(path("aws-scope.json")) and os.path.exists(path("aws")):
            stored = json.load(open(path("aws-scope.json")))
            if stored["secret_fingerprint"] == hashlib.sha256(open(path("aws"), "rb").read()).hexdigest():
                scope = stored["scope"]
        payload = {"type": "aws_credentials_scope", "data": {"scope": scope}}
    elif method == "remove_aws_credentials":
        for name in ("aws", "aws-scope.json"):
            if os.path.exists(path(name)): os.remove(path(name))
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "github_token_state":
        s = state("github")
        payload = {"type": "github_token_state", "data": {"present": s["present"], "fingerprint": s["fingerprint"]}}
    elif method == "install_github_token":
        open(path("github"), "wb").write(base64.b64decode(params["data"]))
        s = state("github")
        payload = {"type": "github_token_state", "data": {"present": True, "fingerprint": s["fingerprint"]}}
    elif method == "skills_state":
        if login_hold:
            open(path("login-holding"), "w").close()
            tries = 0
            while not os.path.exists(login_hold):
                tries += 1
                if tries > 200: sys.exit(1)
                time.sleep(0.05)
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": open(path("skills")).read() if os.path.exists(path("skills")) else "stale"}}
    elif method == "install_skills":
        archive = base64.b64decode(params["data"])
        body = archive[8:]
        if archive[:8] == b"HELSKIL2":
            body = gzip.decompress(body)
        fingerprint = hashlib.sha256(b"HELSKIL1" + body).hexdigest()
        open(path("skills"), "w").write(fingerprint)
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": fingerprint}}
    else:
        raise AssertionError(method)
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;

/// A Claude session on [`AWS_SYNC_RELAY`] whose profile names a role, whose
/// GitHub token comes from `token_file` and whose skills come from `home`.
#[cfg(unix)]
fn aws_sync_session(
    home: &std::path::Path,
    root: &std::path::Path,
    token_file: &std::path::Path,
    aws_cli: &std::path::Path,
) -> CredentialSyncTarget {
    aws_sync_session_named(home, root, token_file, aws_cli, SESSION_ID, None)
}

/// [`aws_sync_session`] for a named session, whose worker may wait for a
/// file before answering about its AWS credentials.
#[cfg(unix)]
fn aws_sync_session_named(
    home: &std::path::Path,
    root: &std::path::Path,
    token_file: &std::path::Path,
    aws_cli: &std::path::Path,
    session_id: &str,
    worker_waits_for: Option<&std::path::Path>,
) -> CredentialSyncTarget {
    std::fs::create_dir_all(home.join("skills/review")).unwrap();
    std::fs::write(home.join("skills/review/SKILL.md"), "review").unwrap();
    let mut target = skills_sync_target(home);
    target.session_id = session_id.into();
    target.skips_login_file_sync = true;
    target.sync_github_token = true;
    target.github = mj_core::config::GithubConfig {
        session_token_path: Some(token_file.to_path_buf()),
        ..Default::default()
    };
    target.skills_scope = mj_core::skills::SkillsScope::Isolated;
    target.spec = CommandSpec::new(
        "python3",
        [
            "-u".to_owned(),
            "-c".to_owned(),
            AWS_SYNC_RELAY.to_owned(),
            RELAY_PROTOCOL_VERSION.to_string(),
            session_id.to_owned(),
            root.to_string_lossy().into_owned(),
        ]
        .into_iter()
        .chain(worker_waits_for.map(|path| path.to_string_lossy().into_owned()))
        .collect::<Vec<_>>(),
    )
    .purpose("AWS sync test relay");
    target.aws_credentials = Some(mj_core::aws_credentials::AwsCredentialSource {
        environment: BTreeMap::from([
            ("PATH".into(), aws_cli.to_string_lossy().into_owned()),
            ("AWS_PROFILE".into(), "bedrock".into()),
        ]),
        role: Some(role_config()),
    });
    target
}

/// An `aws` that refuses every `sts assume-role` with the recorded
/// AccessDenied line.
#[cfg(unix)]
fn refusing_aws_cli(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!(
            "#!/bin/sh\n/bin/cat {} >&2\nexit 254\n",
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/aws-sts-assume-role-access-denied.txt")
                .display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

/// The role the AWS sync tests configure.
#[cfg(unix)]
fn role_config() -> mj_core::config::AwsRoleConfig {
    mj_core::config::AwsRoleConfig {
        role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
        session_policy: None,
        session_policy_arns: Vec::new(),
        duration_seconds: None,
        source_identity: false,
    }
}

/// An `aws` that answers every `sts assume-role` with the recorded output,
/// expiring far in the future.
#[cfg(unix)]
fn granting_aws_cli(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("granting-bin");
    std::fs::create_dir(&bin).unwrap();
    let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/aws-sts-assume-role.json"
    ))
    .unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let output = dir.join("granted.json");
    std::fs::write(&output, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!("#!/bin/sh\nexec /bin/cat {}\n", output.display()),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

/// [`granting_aws_cli`] that takes `millis` to answer and counts its calls in
/// `mints`, next to the stub.
#[cfg(unix)]
fn slow_counting_aws_cli(dir: &std::path::Path, millis: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = granting_aws_cli(dir);
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!(
            "#!/bin/sh\necho x >> {mints}\n/bin/sleep {seconds}\nexec /bin/cat {output}\n",
            mints = dir.join("mints").display(),
            seconds = f64::from(millis) / 1000.0,
            output = dir.join("granted.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_holding_the_hosts_credentials_loses_them_when_its_new_role_fails() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "session-pat").unwrap();
    // Installed while the profile had no role: static host keys, no expiry.
    std::fs::write(
        root.path().join("aws"),
        serde_json::to_vec(&serde_json::json!({
            "Version": 1, "AccessKeyId": "AKIAHOSTKEY", "SecretAccessKey": "host-secret",
        }))
        .unwrap(),
    )
    .unwrap();
    let cli = refusing_aws_cli(root.path());
    let target = aws_sync_session(home.path(), root.path(), &token, &cli);

    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let error = outcomes[0].failure_text().unwrap();
    assert!(error.contains("AccessDenied"), "{error}");
    assert!(
        !root.path().join("aws").exists(),
        "the host's credentials were left installed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_mint_does_not_stop_the_sessions_other_sync_steps() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "rotated-session-pat").unwrap();
    let cli = refusing_aws_cli(root.path());
    let target = aws_sync_session(home.path(), root.path(), &token, &cli);

    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

    // The AWS failure is reported next to what the session's other steps did,
    // not instead of it...
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(
        outcomes[0].failure_text().unwrap().contains("AccessDenied"),
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes[0].outcome,
        Ok(vec![
            CredentialSyncAction::SkillsPushed,
            CredentialSyncAction::GithubTokenPushed
        ])
    );
    // ...so the counters and the failure list both see the session.
    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: outcomes.clone(),
        partial: false,
    };
    assert_eq!(result.skills_sessions(), 1);
    assert_eq!(result.github_token_pushed_sessions(), 1);
    assert_eq!(result.failures().count(), 1);
    // ...and the GitHub token and skills still converged.
    assert_eq!(
        std::fs::read(root.path().join("github")).unwrap(),
        b"rotated-session-pat"
    );
    assert!(
        root.path().join("skills").exists(),
        "skills were not pushed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_github_lookup_does_not_stop_the_aws_step() {
    let home = tempfile::tempdir().unwrap();
    let missing_token = home.path().join("no-such-token-file");

    // Scoped credentials are installed although the token cannot be read...
    let granted_root = tempfile::tempdir().unwrap();
    let cli = granting_aws_cli(granted_root.path());
    let target = aws_sync_session(home.path(), granted_root.path(), &missing_token, &cli);
    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;
    assert!(granted_root.path().join("aws").exists(), "{outcomes:?}");
    assert!(
        outcomes[0].failure_text().unwrap().contains("GitHub token"),
        "{outcomes:?}"
    );

    // ...and the host's credentials are withdrawn although it cannot.
    let refused_root = tempfile::tempdir().unwrap();
    std::fs::write(
        refused_root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    let cli = refusing_aws_cli(refused_root.path());
    let target = aws_sync_session(home.path(), refused_root.path(), &missing_token, &cli);
    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;
    assert!(!refused_root.path().join("aws").exists(), "{outcomes:?}");
    let failure = outcomes[0].failure_text().unwrap();
    assert!(
        failure.contains("GitHub token") && failure.contains("AccessDenied"),
        "{failure}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_secret_is_withdrawn_even_when_the_worker_reports_the_credentials_absent() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    std::fs::write(
        root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    // aws-config is gone: the worker says "absent" about a secret it still has.
    std::fs::write(root.path().join("provider-files-broken"), "").unwrap();
    let cli = refusing_aws_cli(root.path());
    let target = aws_sync_session(home.path(), root.path(), &token, &cli);

    reconcile_profile(std::slice::from_ref(&target), None).await;

    assert!(
        !root.path().join("aws").exists(),
        "the secret file survived"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_slow_session_does_not_hold_up_another_sessions_install() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let fast_root = scratch.path().join("fast");
    let slow_root = scratch.path().join("slow");
    std::fs::create_dir(&fast_root).unwrap();
    std::fs::create_dir(&slow_root).unwrap();
    // The slow session's worker answers only once the fast session holds its
    // credentials, and fails the exchange if that never happens. It is first.
    let targets = [
        aws_sync_session_named(
            home.path(),
            &slow_root,
            &token,
            &cli,
            "slow-session",
            Some(&fast_root.join("aws")),
        ),
        aws_sync_session_named(home.path(), &fast_root, &token, &cli, "fast-session", None),
    ];

    let outcomes = reconcile_profile(&targets, None).await;

    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.step_failures.is_empty() && outcome.outcome.is_ok()),
        "{outcomes:?}"
    );
    assert!(slow_root.join("aws").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn the_workers_recorded_scope_decides_what_a_failed_renewal_keeps() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = refusing_aws_cli(root.path());
    let target = aws_sync_session(home.path(), root.path(), &token, &cli);
    let role_arn = "arn:aws:iam::123456789012:role/mj-bedrock";
    let session_name = format!("mj-{SESSION_ID}");
    let policy_fingerprint = role_config().session_policy_fingerprint();
    // `sign`: made by the controller; unsigned is what a session could write.
    let issued = |expiration: &str,
                  session_name: &str,
                  policy_fingerprint: &str,
                  source_identity: Option<&str>,
                  sign: bool| {
        let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/aws-sts-assume-role.json"
        ))
        .unwrap();
        recorded["Credentials"]["Expiration"] = expiration.into();
        let credentials = mj_core::aws_credentials::AwsCredentials::from_assume_role(
            "bedrock",
            &serde_json::to_vec(&recorded).unwrap(),
        )
        .unwrap()
        .issued_for(role_arn, session_name, policy_fingerprint, source_identity)
        .unwrap();
        if sign {
            credentials
                .sign_scope(crate::aws_assume_role::sign_scope)
                .unwrap()
        } else {
            credentials
        }
    };
    // What a restarted daemon has: no cache, only the configured scope.
    let unavailable = credential_sync::AwsSync::Unavailable {
        error: "AWS refused AssumeRole".into(),
        scoped_required: true,
        expected_scope: Some(credential_sync::ScopeExpectation {
            role_arn: role_arn.into(),
            role_session_name: session_name.clone(),
            session_policy_fingerprint: policy_fingerprint.clone(),
            source_identity: None,
        }),
    };
    let skills = Arc::new(CanonicalSkills::collect(&target));
    let reconcile_for = |aws: credential_sync::AwsSync| {
        let (target, skills) = (&target, &skills);
        async move {
            reconcile_session(
                &SessionRelays::Direct,
                target,
                skills,
                &credential_sync::GithubSync::Known(None),
                &aws,
                &credential_sync::HostSlopcopLogin::Unknown,
                credential_sync::JobHold::new(None, None, None, None),
            )
            .await
        }
    };
    let reconcile = || reconcile_for(unavailable.clone());
    // Install as a worker does, then move the files to where the relay
    // fixture keeps them.
    let install = |credentials: mj_core::aws_credentials::AwsCredentials| {
        credentials.install(root.path(), root.path()).unwrap();
        std::fs::rename(
            root.path()
                .join(mj_core::aws_credentials::SHARED_CREDENTIALS_FILE),
            root.path().join("aws"),
        )
        .unwrap();
        std::fs::rename(
            root.path().join(mj_core::aws_credentials::SCOPE_FILE),
            root.path().join("aws-scope.json"),
        )
        .unwrap();
    };

    // Issued for this session's role and unexpired: kept, failure reported.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        true,
    ));
    let sync = reconcile().await.unwrap().unwrap();
    assert!(root.path().join("aws").exists());
    assert_eq!(
        sync.failures
            .iter()
            .map(|failure| (failure.step, failure.detail.as_str()))
            .collect::<Vec<_>>(),
        [(SyncStep::Aws, "AWS credentials: AWS refused AssumeRole")]
    );
    assert!(
        !sync
            .actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );

    // Issued for another session's name: withdrawn.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        "mj-someone-else",
        &policy_fingerprint,
        None,
        true,
    ));
    let sync = reconcile().await.unwrap().unwrap();
    assert!(!root.path().join("aws").exists());
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );

    // Issued under a broader policy than the one configured now: withdrawn.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        "fingerprint-of-the-policy-before-it-was-narrowed",
        None,
        true,
    ));
    let sync = reconcile().await.unwrap().unwrap();
    assert!(
        !root.path().join("aws").exists(),
        "broader credentials kept"
    );
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );

    // `source_identity` has been enabled since: credentials issued without
    // it are withdrawn, and ones issued with it are kept.
    let wants_source_identity = credential_sync::AwsSync::Unavailable {
        error: "AWS refused AssumeRole".into(),
        scoped_required: true,
        expected_scope: Some(credential_sync::ScopeExpectation {
            role_arn: role_arn.into(),
            role_session_name: session_name.clone(),
            session_policy_fingerprint: policy_fingerprint.clone(),
            source_identity: Some(session_name.clone()),
        }),
    };
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        true,
    ));
    let sync = reconcile_for(wants_source_identity.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !root.path().join("aws").exists(),
        "credentials without the attribution now configured were kept"
    );
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        Some(&session_name),
        true,
    ));
    reconcile_for(wants_source_identity.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(root.path().join("aws").exists());
    // A record the session could have written itself, matching in every
    // field but not made by the controller: withdrawn.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        false,
    ));
    let sync = reconcile().await.unwrap().unwrap();
    assert!(!root.path().join("aws").exists(), "a forged record vouched");
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );
    // The same record with the controller's tag, then tampered with after
    // signing, no longer verifies either.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        true,
    ));
    let stored = root.path().join("aws-scope.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&stored).unwrap()).unwrap();
    // A later expiry: every other field still matches, the tag no longer does.
    record["scope"]["expires_at_epoch_millis"] =
        (record["scope"]["expires_at_epoch_millis"].as_i64().unwrap() + 1000).into();
    std::fs::write(&stored, serde_json::to_vec(&record).unwrap()).unwrap();
    let sync = reconcile().await.unwrap().unwrap();
    assert!(!root.path().join("aws").exists());
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );

    // No scope record at all (the host's own, installed before the role): withdrawn.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        true,
    ));
    std::fs::write(
        root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    let sync = reconcile().await.unwrap().unwrap();
    assert!(!root.path().join("aws").exists());
    assert!(
        sync.actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );

    // The controller cannot read its signing key, so it cannot tell whether
    // the record is its own: that is no evidence of a forgery. The session
    // keeps what it has, and the step says why it could not check.
    install(issued(
        "2099-01-01T00:00:00+00:00",
        &session_name,
        &policy_fingerprint,
        None,
        true,
    ));
    crate::aws_assume_role::SIGNING_KEY_UNAVAILABLE.with(|unavailable| unavailable.set(true));
    let sync = reconcile().await.unwrap().unwrap();
    crate::aws_assume_role::SIGNING_KEY_UNAVAILABLE.with(|unavailable| unavailable.set(false));
    assert!(
        root.path().join("aws").exists(),
        "valid credentials were withdrawn on a key error"
    );
    assert!(
        !sync
            .actions
            .contains(&CredentialSyncAction::AwsCredentialsRemoved)
    );
    let failure = &sync.failures[0].detail;
    assert_eq!(sync.failures.len(), 1, "{:?}", sync.failures);
    assert!(
        failure.contains("could not verify the session's AWS scope record")
            && failure.contains("the signing key is unreadable"),
        "{failure}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_stuck_session_delays_only_its_own_aws_install() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let (stuck_root, fast_root) = (scratch.path().join("stuck"), scratch.path().join("fast"));
    std::fs::create_dir(&stuck_root).unwrap();
    std::fs::create_dir(&fast_root).unwrap();
    // The stuck session's worker answers about its AWS credentials only once
    // this file exists, or fails the exchange after ten seconds.
    let release = scratch.path().join("release-the-stuck-session");
    let targets = vec![
        aws_sync_session_named(
            home.path(),
            &stuck_root,
            &token,
            &cli,
            "stuck-session",
            Some(&release),
        ),
        aws_sync_session_named(home.path(), &fast_root, &token, &cli, "fast-session", None),
    ];
    let pass = tokio::spawn({
        let targets = targets.clone();
        async move { reconcile_profile(&targets, None).await }
    });
    // The fast session's credentials arrive while the pass is still waiting
    // on the stuck session.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fast_root.join("aws").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the fast session waited for the stuck one");
    assert!(!pass.is_finished());

    std::fs::write(&release, "").unwrap();
    let outcomes = pass.await.unwrap();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(stuck_root.join("aws").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_pull_and_a_push_in_one_pass_leave_every_session_on_the_newest_login() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (oldest, middle, newest) = (
        "2026-09-25T20:00:00.000Z",
        "2026-09-25T21:00:00.000Z",
        "2026-09-25T22:00:00.000Z",
    );
    // Canonical holds the middle login. One session is behind it and one is
    // ahead of it, the one that is behind listed first.
    std::fs::write(home.path().join("auth.json"), codex_login(middle)).unwrap();
    let (behind_file, ahead_file) = (scratch.path().join("behind"), scratch.path().join("ahead"));
    std::fs::write(&behind_file, codex_login(oldest)).unwrap();
    std::fs::write(&ahead_file, codex_login(newest)).unwrap();
    let targets = [
        codex_sync_target(home.path(), "behind", oldest, &behind_file),
        codex_sync_target(home.path(), "ahead", newest, &ahead_file),
    ];

    let outcomes = reconcile_profile(&targets, None).await;

    assert!(
        outcomes.iter().all(|o| o.step_failures.is_empty()),
        "{outcomes:?}"
    );
    assert_eq!(
        std::fs::read(home.path().join("auth.json")).unwrap(),
        codex_login(newest),
        "the ahead session's login became canonical"
    );
    assert_eq!(std::fs::read(&behind_file).unwrap(), codex_login(newest));
    assert_eq!(std::fs::read(&ahead_file).unwrap(), codex_login(newest));
}

#[cfg(unix)]
#[tokio::test]
async fn a_matching_secret_without_a_scope_record_is_installed_again() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(root.path());
    // What the mint will return, already on the worker: installed before the
    // worker could record a scope.
    let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/aws-sts-assume-role.json"
    ))
    .unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let minted = mj_core::aws_credentials::AwsCredentials::from_assume_role(
        "bedrock",
        &serde_json::to_vec(&recorded).unwrap(),
    )
    .unwrap();
    std::fs::write(root.path().join("aws"), minted.encode().unwrap()).unwrap();
    let target = aws_sync_session_named(home.path(), root.path(), &token, &cli, "upgraded", None);

    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("aws-scope.json")).unwrap())
            .expect("the scope was recorded on the replacement worker");
    assert_eq!(stored["scope"]["role_session_name"], "mj-upgraded");
    assert_eq!(
        stored["scope"]["session_policy_fingerprint"],
        role_config().session_policy_fingerprint()
    );
    assert!(
        outcomes.iter().any(|outcome| outcome
            .outcome
            .as_ref()
            .is_ok_and(|actions| actions.contains(&CredentialSyncAction::Pushed))),
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_clean_later_pass_replaces_the_failure_an_earlier_one_reported() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let newest = "2026-09-25T22:00:00.000Z";
    let old = "2026-09-25T20:00:00.000Z";
    std::fs::write(home.path().join("auth.json"), codex_login(old)).unwrap();
    let (flaky_file, puller_file) = (scratch.path().join("flaky"), scratch.path().join("puller"));
    std::fs::write(&flaky_file, codex_login(newest)).unwrap();
    std::fs::write(&puller_file, codex_login(newest)).unwrap();
    // The first session's worker refuses its first connection, so the first
    // pass fails it; the second session pulls, which runs a second pass, and
    // that pass finds the first session fine and in agreement.
    let mut flaky = codex_sync_target(home.path(), "flaky", newest, &flaky_file);
    flaky.spec.args.push(
        scratch
            .path()
            .join("failed-once")
            .to_string_lossy()
            .into_owned(),
    );
    let puller = codex_sync_target(home.path(), "puller", newest, &puller_file);

    let outcomes = reconcile_profile(&[flaky, puller], None).await;

    let flaky = outcomes.iter().find(|o| o.session_id == "flaky").unwrap();
    assert_eq!(flaky.outcome, Ok(Vec::new()), "{outcomes:?}");
    assert!(flaky.step_failures.is_empty());
    let puller = outcomes.iter().find(|o| o.session_id == "puller").unwrap();
    assert_eq!(puller.outcome, Ok(vec![CredentialSyncAction::Pulled]));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn the_credential_cache_is_swept_on_the_schedule_even_with_no_sessions() {
    use mj_core::targets::{CommandExecutor, CommandOutput};
    struct ShortLived;
    impl CommandExecutor for ShortLived {
        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            let mut recorded: serde_json::Value = serde_json::from_str(include_str!(
                "../../tests/fixtures/aws-sts-assume-role.json"
            ))
            .unwrap();
            recorded["Credentials"]["Expiration"] = chrono::DateTime::<chrono::Utc>::from(
                std::time::SystemTime::now() + Duration::from_secs(3),
            )
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string()
            .into();
            Ok(CommandOutput {
                status: 0,
                stdout: serde_json::to_vec(&recorded).unwrap(),
                stderr: Vec::new(),
            })
        }
    }
    let source = mj_core::aws_credentials::AwsCredentialSource {
        environment: BTreeMap::from([("AWS_PROFILE".into(), "bedrock".into())]),
        role: Some(role_config()),
    };
    let cache = crate::aws_assume_role::ScopedAwsCredentials::shared();
    cache
        .credentials_for("work", "swept-last-session", &source, &ShortLived)
        .unwrap();
    assert!(cache.holds("swept-last-session"));
    // Its credentials expire; no session is left to sync.
    std::thread::sleep(Duration::from_millis(3300));
    let coordinator = CredentialSyncCoordinator::spawn();
    // Let the coordinator start its schedule before the clock moves.
    tokio::time::sleep(Duration::from_millis(1)).await;
    tokio::time::advance(mj_core::credentials::SYNC_INTERVAL + Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(coordinator);
    assert!(!cache.holds("swept-last-session"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_aws_install_waits_for_its_sessions_login_step_instead_of_being_skipped() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    // The mint takes long enough that the login step is certainly running,
    // and holding the slot, when it completes.
    let cli = slow_counting_aws_cli(scratch.path(), 400);
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let release_login = scratch.path().join("release-the-login-step");
    let mut target = aws_sync_session_named(home.path(), &root, &token, &cli, "busy-login", None);
    // wait_for (none), then the file the login step waits for.
    target.spec.args.push(String::new());
    target
        .spec
        .args
        .push(release_login.to_string_lossy().into_owned());
    // The production path: background admission through a gate.
    super::credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let steps = credential_sync::SessionSteps::default();
    let login = tokio::spawn({
        let (gate, target, steps) = (gate.clone(), target.clone(), steps.clone());
        async move {
            credential_sync::reconcile_profile_guarded(
                &SessionRelays::Direct,
                &[target],
                None,
                Some(&gate),
                None,
                credential_sync::unknown_slopcop_login,
                &steps,
            )
            .await
        }
    });
    let install = tokio::spawn({
        let (gate, target, steps) = (gate.clone(), target.clone(), steps.clone());
        async move {
            credential_sync::aws_cycle(
                &SessionRelays::Direct,
                Some(&gate),
                None,
                &target,
                &steps,
                None,
                false,
            )
            .await
        }
    });
    // The login step holds the session's slot; the mint finishes meanwhile.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("login-holding").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the login step reached the session");
    assert!(gate.is_busy("busy-login"));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !root.join("aws").exists(),
        "installed while the slot was held"
    );

    // The slot frees; the install happens, once.
    std::fs::write(&release_login, "").unwrap();
    login.await.unwrap();
    let (_, installed) = install.await.unwrap();
    assert!(root.join("aws").exists(), "the install was skipped");
    assert!(
        installed
            .unwrap()
            .is_some_and(|sync| sync.actions.contains(&CredentialSyncAction::Pushed))
    );
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("mints"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn aws_errors_are_labelled_by_the_branch_that_failed() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    // The worker drops the connection when asked to install.
    std::fs::write(root.path().join("install-fails"), "").unwrap();
    let granting = granting_aws_cli(root.path());
    let installing = aws_sync_session(home.path(), root.path(), &token, &granting);
    let outcomes = reconcile_profile(std::slice::from_ref(&installing), None).await;
    let failure = outcomes[0].failure_text().unwrap();
    assert!(failure.contains("AWS credentials install"), "{failure}");
    assert!(!failure.contains("withdrawal"), "{failure}");

    // A refused role with a worker that cannot withdraw is the other label.
    let refusing = refusing_aws_cli(root.path());
    let mut withdrawing = aws_sync_session(home.path(), root.path(), &token, &refusing);
    withdrawing.spec.args[3] = "54".into();
    let outcomes = reconcile_profile(std::slice::from_ref(&withdrawing), None).await;
    assert!(
        outcomes[0]
            .failure_text()
            .unwrap()
            .contains("AWS credentials withdrawal"),
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_that_is_no_longer_current_is_not_minted_for() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = slow_counting_aws_cli(scratch.path(), 0);
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "gone-session", None);
    // No daemon says it is current, and nothing on disk lists it.
    super::credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(false));
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());

    let (_, result) = credential_sync::aws_cycle(
        &SessionRelays::Direct,
        Some(&gate),
        None,
        &target,
        &Default::default(),
        None,
        false,
    )
    .await;
    let outcomes = result;

    assert!(
        !scratch.path().join("mints").exists(),
        "AssumeRole was called: {:?}",
        outcomes.as_ref().map(|sync| sync.is_some())
    );
    assert!(!root.join("aws").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_stalled_session_does_not_keep_another_session_from_being_renewed_on_a_later_cycle() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let (stuck_root, ok_root) = (scratch.path().join("stuck"), scratch.path().join("ok"));
    std::fs::create_dir(&stuck_root).unwrap();
    std::fs::create_dir(&ok_root).unwrap();
    // The stuck session never answers about its AWS credentials (until the
    // end of the test). The other one's worker refuses its first install, so
    // its first renewal cycle fails and a second is needed.
    let release = scratch.path().join("release-the-stuck-session");
    std::fs::write(ok_root.join("install-fails"), "").unwrap();
    let mut coordinator =
        CredentialSyncCoordinator::spawn_with_renewal_retry(Duration::from_millis(200));
    coordinator.handle().set_targets(vec![
        aws_sync_session_named(
            home.path(),
            &stuck_root,
            &token,
            &cli,
            "stuck-session",
            Some(&release),
        ),
        aws_sync_session_named(home.path(), &ok_root, &token, &cli, "ok-session", None),
    ]);

    // First cycle: the other session's install fails and says so.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.partial
                && result
                    .failures()
                    .any(|(session, _, _)| session == "ok-session")
            {
                return;
            }
        }
    })
    .await
    .expect("the first renewal cycle reported its failure");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !stuck_root.join("waiting").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stuck session's cycle reached its worker");

    // Second cycle, with the stuck session still stuck: renewed.
    std::fs::remove_file(ok_root.join("install-fails")).unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ok_root.join("aws").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stalled session kept the other one from being renewed");
    assert!(!stuck_root.join("aws").exists());
    std::fs::write(&release, "").unwrap();
}

// An open AWS failure outlives the task that reported it: the replacement
// task's first clean cycle is reported too, so the notices end the failure and
// report a later recurrence.
#[cfg(unix)]
#[tokio::test]
async fn a_replaced_renewal_task_still_ends_the_failure_its_predecessor_reported() {
    use crate::pollers::CredentialSyncNotices;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("install-fails"), "").unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "replaced", None);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let next_partial = async |coordinator: &mut CredentialSyncCoordinator| {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let result = coordinator.result().await.expect("coordinator stopped");
                if result.partial {
                    return result;
                }
            }
        })
        .await
        .expect("a renewal result arrived")
    };
    let mut notices = CredentialSyncNotices::default();
    let state = mj_core::state::State::default();

    let failed = next_partial(&mut coordinator).await;
    assert!(failed.failures().next().is_some(), "{failed:?}");
    assert!(notices.notice(&failed, None, &state).is_some());

    // The target changes (a role edit): its task is replaced, and the worker
    // now accepts the install.
    std::fs::remove_file(root.join("install-fails")).unwrap();
    let mut changed = target.clone();
    changed
        .aws_credentials
        .as_mut()
        .unwrap()
        .environment
        .insert("AWS_REGION".into(), "us-east-2".into());
    handle.set_targets(vec![changed]);
    let clean = next_partial(&mut coordinator).await;
    assert!(clean.failures().next().is_none(), "{clean:?}");
    assert_eq!(notices.notice(&clean, None, &state), None);

    // The failure ended, so it is reported again when it recurs.
    assert!(notices.notice(&failed, None, &state).is_some());
}

// A renewal task stopped while its job waits on the session's actor takes the
// job's hold with it (the actor drops the job unrun); stopped while the actor
// is already running the job, the hold stays with the job until it ends, so a
// replacement task's job never overlaps it.
#[cfg(unix)]
#[tokio::test]
async fn a_stopped_renewals_job_is_dropped_if_queued_and_finishes_if_running() {
    use crate::session_manager::RelayConnectionJob;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let release = scratch.path().join("release-the-worker");
    // The worker holds its first AWS answer until `release` exists.
    let target =
        aws_sync_session_named(home.path(), &root, &token, &cli, SESSION_ID, Some(&release));
    let session = target.session_id.clone();
    // The job, if it ran, would withdraw these.
    std::fs::write(root.join("aws"), "host credentials").unwrap();
    super::credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let steps = credential_sync::SessionSteps::default();
    let queue = || async {
        let work =
            credential_sync::SyncWork::Aws(Box::new(credential_sync::AwsSync::Unavailable {
                error: "no credentials".into(),
                scoped_required: true,
                expected_scope: None,
            }));
        let hold = credential_sync::JobHold::new(
            gate.try_start(&session),
            Some(steps.enter(&session).await),
            None,
            None,
        );
        credential_sync::CredentialSyncJob::queue(target.clone(), work, hold)
    };

    // Queued: the awaiting task stops, the hold is released at once, and the
    // actor, when it gets to the job, drops it.
    let (job, response, queued) = queue().await;
    drop((response, queued));
    assert!(!gate.is_busy(&session));
    tokio::time::timeout(Duration::from_secs(5), steps.enter(&session))
        .await
        .expect("the turn was not released with the abandoned job");
    let mut client = RelayClient::connect(&target.spec, &session).await.unwrap();
    Box::new(job).run(&mut client).await;
    assert!(root.join("aws").exists(), "an abandoned job ran");
    drop(client);

    // Running: the awaiting task stops while the actor talks to the worker.
    // The job keeps the session until it ends.
    let (job, response, queued) = queue().await;
    let mut client = RelayClient::connect(&target.spec, &session).await.unwrap();
    let running = tokio::spawn(async move { Box::new(job).run(&mut client).await });
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.join("waiting").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the job reached the worker");
    drop((response, queued));
    assert!(gate.is_busy(&session), "a running job lost the session");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), steps.enter(&session))
            .await
            .is_err(),
        "a running job lost its turn"
    );
    std::fs::write(&release, "").unwrap();
    running.await.unwrap();
    assert!(!gate.is_busy(&session));
    assert!(
        !root.join("aws").exists(),
        "the running job did not finish its work"
    );
}

// A job queued behind an actor that is busy with something else holds the
// session's admission and turn only until the gate cancels the admission: a
// close, restart or move waiting for the session (and a daemon handoff behind
// it) must not wait for the actor to reach a job nobody wants.
#[cfg(unix)]
#[tokio::test]
async fn a_queued_job_releases_its_hold_as_soon_as_admission_is_cancelled() {
    use crate::session_manager::RelayConnectionJob;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session(home.path(), &root, &token, &cli);
    let session = target.session_id.clone();
    std::fs::write(root.join("aws"), "host credentials").unwrap();
    super::credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let steps = credential_sync::SessionSteps::default();

    // The task that waits for the job: it queues it (here, hands it to the
    // test, which plays the busy actor) and waits for the answer.
    let (queue_tx, queue_rx) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn({
        let (gate, steps, target) = (gate.clone(), steps.clone(), target.clone());
        async move {
            let turn = steps.enter(&target.session_id).await;
            let for_admission = target.clone();
            credential_sync::admitted(
                Some(&gate),
                None,
                &for_admission,
                turn,
                None,
                |hold| async move {
                    let work = credential_sync::SyncWork::Aws(Box::new(
                        credential_sync::AwsSync::Unavailable {
                            error: "no credentials".into(),
                            scoped_required: true,
                            expected_scope: None,
                        },
                    ));
                    let (job, response, _queued) =
                        credential_sync::CredentialSyncJob::queue(target, work, hold);
                    queue_tx.send(job).ok();
                    let _ = response.await;
                    Ok::<Option<()>, anyhow::Error>(None)
                },
            )
            .await
        }
    });
    let job = queue_rx.await.unwrap();
    assert!(gate.is_busy(&session));

    // A lifecycle operation cancels the admission and waits for it to free.
    gate.cancel_busy(&session);
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.is_busy(&session) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the queued job kept the session's admission after it was cancelled");
    tokio::time::timeout(Duration::from_secs(5), steps.enter(&session))
        .await
        .expect("the queued job kept the session's turn");
    waiting.await.unwrap().unwrap();

    // The actor reaches the job at last and drops it unrun.
    let mut client = RelayClient::connect(&target.spec, &session).await.unwrap();
    Box::new(job).run(&mut client).await;
    assert!(root.join("aws").exists(), "a cancelled job ran");
}

// An authentication failure may mean the session's credentials are no longer
// good: the session's renewal task renews at once instead of at half the
// credentials' lifetime, and the trigger's result says what it did.
#[cfg(unix)]
#[tokio::test]
async fn an_auth_trigger_reinstalls_deleted_scoped_credentials_at_once() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    // Granted for decades: only the trigger can make the renewal early.
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "auth-session", None);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.partial && result.pushed_to("auth-session") {
                return;
            }
        }
    })
    .await
    .expect("the first install was reported");

    // The worker lost them (a replaced worker, a cleared home).
    std::fs::remove_file(root.join("aws")).unwrap();
    std::fs::remove_file(root.join("aws-scope.json")).unwrap();
    handle.sync_profile_now(
        &target.profile_id,
        Some(mj_core::credentials::CredentialSyncCause {
            session_id: "auth-session".into(),
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
        }),
    );
    let triggered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.trigger.is_some() {
                return result;
            }
        }
    })
    .await
    .expect("the trigger's result arrived");
    assert!(
        root.join("aws").exists(),
        "the credentials were not reinstalled"
    );
    assert!(root.join("aws-scope.json").exists());
    assert!(triggered.pushed_to("auth-session"), "{triggered:?}");
    assert!(!triggered.partial);
}

// The lock map has an entry per session only while the session's lock is held
// or awaited, and removing a finished entry never lets a waiter and a newcomer
// hold the session at once.
#[tokio::test]
async fn session_turns_are_exclusive_and_leave_no_entry_behind() {
    let steps = credential_sync::SessionSteps::default();
    let mut turns = Vec::new();
    for session in 0..2000 {
        turns.push(steps.enter(&format!("session-{session}")).await);
    }
    assert_eq!(steps.tracked(), 2000);
    drop(turns);
    assert_eq!(steps.tracked(), 0);

    // A holds; B waits; A releases. The entry must survive for B, so that C
    // still finds the lock B holds.
    let first = steps.enter("shared").await;
    let waiting = tokio::spawn({
        let steps = steps.clone();
        async move { steps.enter("shared").await }
    });
    tokio::task::yield_now().await;
    drop(first);
    let second = waiting.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), steps.enter("shared"))
            .await
            .is_err(),
        "a newcomer entered while the waiter held the session"
    );
    drop(second);
    assert_eq!(steps.tracked(), 0);

    // A waiter that gives up leaves nothing behind either.
    let held = steps.enter("abandoned").await;
    let _ = tokio::time::timeout(Duration::from_millis(50), steps.enter("abandoned")).await;
    drop(held);
    assert_eq!(steps.tracked(), 0);
}

#[test]
fn a_failed_login_step_keeps_the_aws_steps_actions_in_the_outcome() {
    let mut outcomes = BTreeMap::from([(
        "session".to_owned(),
        CredentialSyncOutcome {
            session_id: "session".into(),
            outcome: Err("worker proxy disconnected".into()),
            step_failures: Vec::new(),
        },
    )]);

    credential_sync::merge_step_outcome(
        &mut outcomes,
        "session",
        vec![CredentialSyncAction::Pushed],
        Vec::new(),
    );

    let merged = &outcomes["session"];
    assert_eq!(merged.outcome, Ok(vec![CredentialSyncAction::Pushed]));
    assert_eq!(
        merged.failure_text().as_deref(),
        Some("worker proxy disconnected")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_worker_before_protocol_55_is_left_to_the_automatic_upgrade_and_then_withdrawn() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    std::fs::write(
        root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    let cli = refusing_aws_cli(root.path());
    let mut old = aws_sync_session(home.path(), root.path(), &token, &cli);
    // The relay fixture's protocol is its fourth argument.
    old.spec.args[3] = "54".into();

    let outcomes = reconcile_profile(std::slice::from_ref(&old), None).await;

    // Nothing is asked of the person. The worker upgrade coordinator replaces
    // a worker whose build differs from the controller's when its session is
    // idle (`worker_upgrade::tests`: a quiet session with an old build is
    // due, a busy one waits), keeping the home.
    let failure = outcomes[0].failure_text().unwrap();
    assert!(
        failure.contains("replacing that worker automatically"),
        "{failure}"
    );
    assert!(!failure.contains("restart"), "no manual step: {failure}");
    // The refused mint and the withdrawal that could not happen are both
    // said, in one failure for the step, the withdrawal first.
    assert_eq!(outcomes[0].step_failures.len(), 1, "{failure}");
    let (withdrawal, mint) = (
        failure.find("predates withdrawing").expect(&failure),
        failure.find("AccessDenied").expect(&failure),
    );
    assert!(withdrawal < mint, "{failure}");
    assert!(
        root.path().join("aws").exists(),
        "an old worker cannot withdraw"
    );

    // On the replacement, the same session's next pass withdraws.
    let current = aws_sync_session(home.path(), root.path(), &token, &cli);
    reconcile_profile(std::slice::from_ref(&current), None).await;
    assert!(!root.path().join("aws").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn the_coordinator_runs_a_trigger_for_a_held_session_once_it_is_free() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    // The first pass holds the session until this file exists.
    let release = scratch.path().join("release");
    let target = aws_sync_session_named(
        home.path(),
        &root,
        &token,
        &cli,
        "held-session",
        Some(&release),
    );
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target]);
    // The worker announces that it is handling the first pass's request.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("waiting").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first pass reached the session");
    handle.sync_profile_now(
        "work",
        Some(mj_core::credentials::CredentialSyncCause {
            session_id: "held-session".into(),
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
        }),
    );
    std::fs::write(&release, "").unwrap();

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.trigger.is_some() {
                return result;
            }
        }
    })
    .await
    .expect("the trigger was dropped");
    assert!(
        result
            .outcomes
            .iter()
            .any(|outcome| outcome.session_id == "held-session"),
        "{result:?}"
    );
}

/// #1160: a sync that a session's auth failure asked for says whether it
/// reached that session, even when the session's login already matched the
/// profile's. That match is what shows the profile's own login is the one the
/// provider refused, so a spawn on the profile can be refused at once. A
/// periodic sync still leaves sessions that agreed out of its outcomes.
// Hard-won: 1b077959: refused profile logins left sub-agent failures unrecognized and retriggered.
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
            step_failures: Vec::new(),
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
                step_failures: Vec::new(),
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

// Hard-won: c8d27f3c: credential sync connected after lifecycle had parked the worker and reported ECONNREFUSED.
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
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
        spec: CommandSpec::new("must-not-start-a-proxy", Vec::<String>::new()),
    };
    let result = credential_sync::reconcile_profile_guarded(
        &SessionRelays::Direct,
        &[target],
        Some(SESSION_ID),
        Some(&gate),
        None,
        credential_sync::unknown_slopcop_login,
        &Default::default(),
    )
    .await;
    assert!(
        result.is_empty(),
        "deferral must not report an unchanged or failed login"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cpu_usage_skips_protocol_28_without_writing_a_request() {
    let script = format!(
        r#"python3 -c '
import json, sys
req = json.loads(sys.stdin.readline())
print(json.dumps({{"request_id":req["request_id"], "protocol_version":28, "result":"ok", "payload":{{"type":"hello", "data":{{"negotiated":28, "relay_version":"cpu-old", "session_id":{session:?}}}}}}}), flush=True)
sys.stdin.read()
'"#,
        session = SESSION_ID
    );
    let spec = CommandSpec::new("sh", ["-c", &script]).purpose("old CPU relay fixture");
    let mut client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .unwrap();
    let before = client.next_request;
    assert_eq!(client.cpu_usage().await.unwrap(), None);
    assert_eq!(client.next_request, before);
}

// Hard-won: adc0d368: a worker whose harness preparation failed rejected
// sub-agent polling, so its session could not sync, suspend or close.
#[cfg(unix)]
#[tokio::test]
async fn sub_agent_polling_preserves_legacy_preparation_handling_and_reports_feed_errors() {
    for (protocol, code, accepted) in [
        (37, RelayErrorCode::InvalidState, true),
        (37, RelayErrorCode::Internal, false),
        (RELAY_PROTOCOL_VERSION, RelayErrorCode::InvalidState, false),
        (RELAY_PROTOCOL_VERSION, RelayErrorCode::Internal, false),
    ] {
        let rejection = serde_json::to_value(RelayResponseEnvelope {
            request_id: "REQUEST_ID".into(),
            protocol_version: protocol,
            body: RelayResponseBody::Error {
                error: RelayProtocolError {
                    code,
                    message: "harness preparation failed at harness-profile: missing staged \
                              Claude delegation server"
                        .into(),
                    retryable: false,
                    detail: None,
                },
            },
        })
        .unwrap()
        .to_string();
        let script = format!(
            r#"
import json, sys
rejection = {rejection:?}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {{"type": "hello", "data": {{"negotiated": {protocol}, "relay_version": "preparation-fixture", "session_id": "{SESSION_ID}"}}}}
        print(json.dumps({{"request_id": req["request_id"], "protocol_version": {protocol}, "result": "ok", "payload": payload}}), flush=True)
    elif method in ("subagent_requests", "poll_subagent_queue"):
        print(rejection.replace("REQUEST_ID", req["request_id"]), flush=True)
    else:
        raise AssertionError(method)
"#
        );
        let spec =
            CommandSpec::new("python3", ["-c", &script]).purpose("failed preparation fixture");
        let mut client =
            RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
                .await
                .unwrap();
        let result = client.subagent_requests().await;
        if accepted {
            let (requests, results) = result.unwrap();
            assert!(requests.is_empty() && results.is_empty());
        } else {
            assert!(result.is_err(), "{code:?} must still fail the sync");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn project_memory_wire_format_tracks_worker_protocol_and_decodes_both_forms() {
    use mj_core::project_memory::{ProjectMemorySnapshot, ReplicaReplaceOutcome, TreeVersion};
    let tree = ProjectMemorySnapshot {
        files: BTreeMap::from([("/MEMORY.md".into(), "memory notes\n".repeat(250_000))]),
    };
    // An independent Python peer checks both request variants and supplies
    // each response encoding, even when it differs from the negotiated one.
    let script = r#"
import base64, gzip, json, sys
protocol, session, response_compressed = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
tree = {"files": {"/MEMORY.md": "memory notes\n" * 250000}}
def decode(wire):
    if protocol < 44:
        assert wire == tree
        assert list(wire) == ["files"]
        return wire
    assert list(wire) == ["gzip"]
    return {"files": json.loads(gzip.decompress(base64.b64decode(wire["gzip"])))}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "memory-fixture", "session_id": session}}
    elif method == "install_project_memory_snapshot":
        assert decode(req["request"]["params"]["snapshot"]) == tree
        payload = {"type": "project_memory_snapshot_installed"}
    elif method == "replace_project_memory_tree":
        assert decode(req["request"]["params"]["tree"]) == tree
        payload = {"type": "project_memory_tree_replaced", "data": {"outcome": "replaced"}}
    elif method == "project_memory_snapshot":
        wire = {"gzip": base64.b64encode(gzip.compress(json.dumps(tree["files"]).encode())).decode()} if response_compressed else tree
        payload = {"type": "project_memory_snapshot", "data": {"baseline": wire, "replica": wire}}
    else:
        raise AssertionError(method)
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;
    for protocol in [43, 44] {
        for response_compressed in [0, 1] {
            let spec = CommandSpec::new(
                "python3",
                [
                    "-c".to_owned(),
                    script.to_owned(),
                    protocol.to_string(),
                    SESSION_ID.to_owned(),
                    response_compressed.to_string(),
                ],
            )
            .purpose("project memory protocol compatibility fixture");
            let mut client =
                RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10))
                    .await
                    .unwrap();
            client
                .install_project_memory_snapshot(tree.clone())
                .await
                .unwrap();
            assert_eq!(
                client
                    .replace_project_memory_tree(TreeVersion("expected".into()), tree.clone())
                    .await
                    .unwrap(),
                ReplicaReplaceOutcome::Replaced
            );
            assert_eq!(
                client.project_memory_snapshot().await.unwrap(),
                (tree.clone(), tree.clone())
            );
        }
    }
}

// Hard-won: ac9d7ce9f: empty delegation deltas rebuilt and copied full queue payloads.
#[cfg(unix)]
#[tokio::test]
async fn delegation_publications_share_empty_polls_and_recover_partial_pages() {
    use mj_core::subagent::{
        SubagentCompletion, SubagentQueueChange, SubagentQueuePage, SubagentToolAction,
        SubagentToolRequest,
    };
    let completion = |id: &str| SubagentCompletion {
        request_id: id.into(),
        completed_at_ms: 100,
        is_error: true,
        input_delivery: Some(serde_json::json!({"error": "failure detail\n".repeat(10_000)})),
        interrupted_wait: None,
    };
    let first = completion("first");
    let second = completion("second");
    let request = SubagentToolRequest {
        originating_command_id: None,
        request_id: "second".into(),
        created_at_ms: 100,
        action: SubagentToolAction::SendMessage {
            child_session_id: "child".into(),
            message: "message detail\n".repeat(10_000),
        },
    };
    let page = |sequence, changes, has_more| RelayResponseBody::Ok {
        payload: RelayResponsePayload::SubagentQueue {
            page: SubagentQueuePage {
                sequence,
                changes,
                has_more,
            },
        },
    };
    let replies = vec![
        page(
            1,
            vec![SubagentQueueChange::Completed(first.clone())],
            false,
        ),
        page(2, vec![], false),
        page(
            3,
            vec![
                SubagentQueueChange::Retired {
                    request_id: "first".into(),
                },
                SubagentQueueChange::Request(request.clone()),
            ],
            true,
        ),
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                message: "failure between queue pages".into(),
                retryable: true,
                detail: None,
            },
        },
        page(4, vec![], false),
        page(5, vec![], false),
        page(
            6,
            vec![SubagentQueueChange::Completed(second.clone())],
            false,
        ),
        page(
            7,
            vec![SubagentQueueChange::Retired {
                request_id: "second".into(),
            }],
            false,
        ),
        page(8, vec![], false),
    ];
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("queue-pages.json");
    let encoded = serde_json::to_vec(&replies).unwrap();
    assert!(encoded.len() > 64 * 1024);
    std::fs::write(&fixture, encoded).unwrap();
    let script = format!(
        r#"
import json, sys
pages = iter(json.load(open(sys.argv[1])))
for line in sys.stdin:
    request = json.loads(line)
    method = request["request"]["method"]
    if method == "hello":
        body = {{"result": "ok", "payload": {{"type": "hello", "data": {{"negotiated": {protocol}, "relay_version": "queue-publication-fixture", "session_id": "{SESSION_ID}"}}}}}}
    else:
        assert method == "poll_subagent_queue", method
        body = next(pages)
    body.update(request_id=request["request_id"], protocol_version={protocol})
    print(json.dumps(body), flush=True)
"#,
        protocol = RELAY_PROTOCOL_VERSION
    );
    let spec = CommandSpec::new("python3", ["-c", &script, &fixture.to_string_lossy()])
        .purpose("delegation queue publication fixture");
    let mut client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(10))
        .await
        .unwrap();
    let held = client.subagent_requests().await.unwrap();
    assert_eq!(&*held.1, std::slice::from_ref(&first));
    let unchanged = client.subagent_requests().await.unwrap();
    assert!(held.0.ptr_eq(&unchanged.0) && held.1.ptr_eq(&unchanged.1));
    let error = client.subagent_requests().await.unwrap_err();
    assert!(
        format!("{error:#}").contains("failure between queue pages"),
        "{error:#}"
    );
    assert!(client.subagent_publication.is_none());
    let recovered = client.subagent_requests().await.unwrap();
    assert_eq!(&*recovered.0, &[request]);
    assert!(recovered.1.is_empty());
    assert_eq!(
        &*held.1,
        &[first],
        "held completion stays immutable after retirement"
    );
    let unchanged = client.subagent_requests().await.unwrap();
    assert!(recovered.0.ptr_eq(&unchanged.0) && recovered.1.ptr_eq(&unchanged.1));
    let completed = client.subagent_requests().await.unwrap();
    assert!(completed.0.is_empty());
    assert_eq!(&*completed.1, &[second]);
    let retired = client.subagent_requests().await.unwrap();
    assert!(retired.0.is_empty() && retired.1.is_empty());
    let unchanged = client.subagent_requests().await.unwrap();
    assert!(retired.0.ptr_eq(&unchanged.0) && retired.1.ptr_eq(&unchanged.1));
}
