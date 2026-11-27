pub use super::test_fixtures::*;
use super::*;
#[cfg(unix)]
use crate::test_log::CapturedLog;
use mj_core::relay::RelayObservation;
use mj_worker::relay::DurableRelay;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the result installed AWS credentials on the session.
#[cfg(unix)]
fn aws_pushed(result: &CredentialSyncResult, session_id: &str) -> bool {
    result.outcomes.iter().any(|outcome| {
        outcome.session_id == session_id
            && outcome
                .outcome
                .as_ref()
                .is_ok_and(|actions| actions.contains(&CredentialSyncAction::AwsCredentialsPushed))
    })
}

/// What a direct (no actor) reconciliation of one session borrows.
#[cfg(unix)]
macro_rules! access {
    () => {
        credential_sync::SessionAccess {
            relays: &SessionRelays::Direct,
            runtime: None,
        }
    };
}

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
        crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            test,
        ))
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
    // The lifecycle/actor caller reports the failure, without a duplicate here.
    assert!(log.at_or_above(tracing::Level::WARN).is_empty());
    let warnings = log
        .at(tracing::Level::DEBUG)
        .into_iter()
        .filter(|event| event.contains("relay request failed"))
        .collect::<Vec<_>>();
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
    let mut spec = relay_proxy_before_the_worker_binds(directory.path(), Some(2));
    // Hard-won: 583e6ea48: a stale socket after park returns ECONNREFUSED until the new worker binds.
    spec.args = spec
        .args
        .into_iter()
        .map(|arg| arg.replace("No such file or directory", "Connection refused"))
        .collect();

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

/// A socket still missing after the retry budget returns the dead-transport
/// error to the caller that owns recovery and reachability warnings.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_socket_still_missing_returns_the_failure_to_its_owner() {
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

    assert!(log.at_or_above(tracing::Level::WARN).is_empty());
    let warnings = log
        .at(tracing::Level::DEBUG)
        .into_iter()
        .filter(|event| event.contains("control socket is still missing"))
        .collect::<Vec<_>>();
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
        crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            "a_missing_worker_socket_retries_on_the_same_shard",
        ))
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
    // Hard-won: 583e6ea48: embedded worker ERROR lines and backtraces were pasted into daemon warnings.
    let stderr = format!(
        "2026-11-17T13:45:12Z ERROR mj_worker: worker failed\nError: {COMPLAINT}\n\nCaused by:\n    0: connect unix socket /x/control.sock\n    1: path must be shorter than SUN_LEN"
    );
    let spec = CommandSpec::new(
        "sh",
        ["-c", &format!("printf '%s\\n' '{stderr}' >&2; exit 1")],
    )
    .purpose("complaining relay proxy");

    let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .err()
        .expect("a proxy that exits cannot complete hello");

    assert!(format!("{error:#}").contains(COMPLAINT), "{error:#}");
    assert!(!format!("{error:#}").contains('\n'), "{error:#}");
    assert!(
        !format!("{error:#}").contains("ERROR mj_worker"),
        "{error:#}"
    );
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
        .call_with_timeout(RelayRequest::Status, Duration::from_millis(500))
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

#[tokio::test(start_paused = true)]
async fn observation_deadline_tolerates_load_and_distinguishes_eof() {
    let slow_reply = Duration::from_secs(25);
    for (status, delay, reply) in [
        (true, slow_reply, true),
        (false, slow_reply, true),
        (false, slow_reply, false),
        (
            false,
            RELAY_OBSERVATION_TIMEOUT + Duration::from_secs(1),
            false,
        ),
    ] {
        let (controller, worker) = tokio::io::duplex(4096);
        let mut client = duplex_relay_client(controller, SESSION_ID);
        let peer = mj_core::runtime::spawn(async move {
            let (reader, mut writer) = tokio::io::split(worker);
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            let payload = match request.request {
                RelayRequest::Status if status => RelayResponsePayload::Status(
                    mj_core::relay::RelaySnapshot::new(SESSION_ID.into()).operational_state(),
                ),
                RelayRequest::PollSubagentQueue { acknowledged: None } if !status => {
                    RelayResponsePayload::SubagentQueue {
                        page: mj_core::subagent::SubagentQueuePage {
                            sequence: 1,
                            changes: Vec::new(),
                            has_more: false,
                        },
                    }
                }
                other => panic!("unexpected request: {}", other.method_name()),
            };
            tokio::time::sleep(delay).await;
            if reply {
                let mut body = serde_json::to_vec(&RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version: request.protocol_version,
                    body: RelayResponseBody::Ok { payload },
                })
                .unwrap();
                body.push(b'\n');
                writer.write_all(&body).await.unwrap();
            }
        });
        let start = tokio::time::Instant::now();
        let result = if status {
            client
                .status()
                .await
                .map(|state| assert_eq!(state.session_id, SESSION_ID))
        } else {
            client
                .subagent_requests()
                .await
                .map(|(requests, _)| assert!(requests.is_empty()))
        };
        let elapsed = start.elapsed();
        let expected = delay.min(RELAY_OBSERVATION_TIMEOUT);
        assert!(
            (expected..expected + Duration::from_millis(10)).contains(&elapsed),
            "{elapsed:?}"
        );
        if reply {
            result.unwrap();
        } else if delay < RELAY_OBSERVATION_TIMEOUT {
            assert!(RelayTransportDead::marks(&result.unwrap_err()));
        } else {
            assert!(!RelayTransportDead::marks(&result.unwrap_err()));
            let error = client.subagent_requests().await.unwrap_err();
            assert!(format!("{error:#}").contains("connection abandoned"));
            assert_eq!(start.elapsed(), elapsed);
        }
        peer.await.unwrap();
    }
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
        let selected = crate::github_app::github_token_for_session(
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
        assert_eq!(changed.outcomes.len(), 1);
        assert_eq!(changed.outcomes[0].session_id, child.session_id);

        // Verify that the explicit authentication trigger still reaches the
        // untouched profile.
        handle.sync_profile_now(
            "other",
            Some(mj_core::credentials::CredentialSyncCause {
                session_id: other.session_id.clone(),
                ordinal: 1,
                reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
                login_fingerprint: None,
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
    let write = mj_core::runtime::spawn(async move {
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
    let page = RelayEventPage {
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
            credential_sync::HostLogins {
                slopcop: &credential_sync::HostSlopcopLogin::Unknown,
                harness: &credential_sync::PassLogin::PerSession,
                pinned: None,
            },
            credential_sync::JobHold::new(None, None),
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
canonical_path = sys.argv[8] if len(sys.argv) > 8 else ""
new_canonical_path = sys.argv[9] if len(sys.argv) > 9 else ""
worker_refresh_path = sys.argv[10] if len(sys.argv) > 10 else ""
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    if method == "hello":
        if fail_first and not os.path.exists(fail_first):
            open(fail_first, "w").close()
            sys.exit(1)
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "credential-fixture", "session_id": session}}
    elif method == "status" and os.environ.get("MJ_TEST_CREDENTIAL_ACTOR_STATE"):
        payload = {"type": "status", "data": json.loads(os.environ["MJ_TEST_CREDENTIAL_ACTOR_STATE"])}
    elif method == "attach" and os.environ.get("MJ_TEST_CREDENTIAL_ACTOR_STATE"):
        state = json.loads(os.environ["MJ_TEST_CREDENTIAL_ACTOR_STATE"])
        payload = {"type": "attached", "data": {"state": state, "events": [], "through_ordinal": 0, "through_digest": state["latest_digest"]}}
    elif method == "acknowledge" and os.environ.get("MJ_TEST_CREDENTIAL_ACTOR_STATE"):
        state = json.loads(os.environ["MJ_TEST_CREDENTIAL_ACTOR_STATE"])
        payload = {"type": "acknowledged", "data": {"through_ordinal": 0, "through_digest": state["latest_digest"]}}
    elif method == "history_requests" and os.environ.get("MJ_TEST_CREDENTIAL_ACTOR_STATE"):
        payload = {"type": "history_requests", "data": {"requests": []}}
    elif method == "subagent_requests" and os.environ.get("MJ_TEST_CREDENTIAL_ACTOR_STATE"):
        payload = {"type": "subagent_requests", "data": {"requests": [], "results": []}}
    elif method == "credential_state":
        if os.environ.get("MJ_TEST_CREDENTIAL_WAITING"):
            import time
            waiting = os.environ["MJ_TEST_CREDENTIAL_WAITING"]
            release = os.environ["MJ_TEST_CREDENTIAL_RELEASE"]
            open(waiting, "w").close()
            while os.path.isdir(os.path.dirname(release)) and not os.path.exists(release):
                time.sleep(0.01)
        if os.path.exists(received):
            data = open(received, "rb").read()
            login = json.loads(data)
            if isinstance(login, dict) and login.get("last_refresh"):
                import datetime
                fingerprint = hashlib.sha256(data).hexdigest()
                freshness = int(datetime.datetime.fromisoformat(login["last_refresh"].replace("Z", "+00:00")).timestamp() * 1000)
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
        # The AWS profile the session was staged with outlives the credentials.
        profile_file = received + ".profile"
        profile = open(profile_file).read() if os.path.exists(profile_file) else None
        payload = {"type": "aws_credentials_scope", "data": {"scope": scope, "profile": profile}}
    elif method == "read_credentials":
        payload = {"type": "credentials", "data": {"data": base64.b64encode(open(received, "rb").read()).decode()}}
        if canonical_path and new_canonical_path:
            import shutil
            shutil.copyfile(new_canonical_path, canonical_path)
    elif method == "remove_aws_credentials":
        import pathlib
        pathlib.Path(received).unlink(missing_ok=True)
        pathlib.Path(received + ".scope").unlink(missing_ok=True)
        payload = {"type": "credential_state", "data": {"present": False, "fingerprint": "", "freshness_epoch_ms": None}}
    elif method in ("install_credentials", "install_aws_credentials"):
        data = base64.b64decode(req["request"]["params"]["data"])
        if method == "install_credentials" and os.environ.get("MJ_TEST_CREDENTIAL_INSTALLS"):
            with open(os.environ["MJ_TEST_CREDENTIAL_INSTALLS"], "a") as attempts:
                attempts.write("install\n")
        if method == "install_credentials" and worker_refresh_path:
            data = open(worker_refresh_path, "rb").read()
            worker_refresh_path = ""
        with open(received, "wb") as out:
            out.write(data)
        if method == "install_aws_credentials":
            open(received + ".profile", "w").write(json.loads(data)["Profile"])
            scope = req["request"]["params"].get("scope")
            if scope:
                json.dump(scope, open(received + ".scope", "w"))
        installed_freshness = None
        if method == "install_credentials":
            import datetime
            login = json.loads(data)
            refreshed = login.get("last_refresh")
            installed_freshness = int(datetime.datetime.fromisoformat(refreshed.replace("Z", "+00:00")).timestamp() * 1000) if refreshed else None
            fingerprint, freshness = hashlib.sha256(data).hexdigest(), installed_freshness
        payload = {"type": "credential_state", "data": {"present": True, "fingerprint": hashlib.sha256(data).hexdigest(), "freshness_epoch_ms": installed_freshness}}
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
    // Staged, as launch does, under the profile the host environment names in
    // these tests.
    std::fs::write(format!("{}.profile", received.display()), "bedrock").unwrap();
    target
}

// Hard-won: #1385: AWS reconciliation skipped credentials, and local sessions can share the same profile.
#[cfg(unix)]
#[tokio::test]
async fn unscoped_aws_auth_wake_reexports_only_for_the_isolated_session() {
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
        mints: true,
    });
    let targets = [local, remote];
    let write_export = |generation: &str| {
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
    };
    let received_key = || {
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&remote_received).unwrap()).unwrap();
        received["AccessKeyId"].as_str().unwrap().to_owned()
    };
    write_export("initial");
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    let profile_id = targets[1].profile_id.clone();
    handle.set_targets(targets.into_iter().collect());
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.unwrap();
            if result.partial && aws_pushed(&result, "remote") {
                break;
            }
        }
    })
    .await
    .expect("initial export was installed");
    assert_eq!(received_key(), "key-initial");

    write_export("renewed");
    let cause = mj_core::credentials::CredentialSyncCause {
        session_id: "remote".into(),
        ordinal: 1,
        reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
        login_fingerprint: None,
    };
    handle.sync_profile_now(&profile_id, Some(cause.clone()));
    let triggered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.unwrap();
            if result.trigger.is_some() {
                break result;
            }
        }
    })
    .await
    .expect("the authentication wake reported its new export");
    assert_eq!(triggered.trigger, Some(cause));
    assert!(aws_pushed(&triggered, "remote"), "{triggered:?}");
    assert_eq!(received_key(), "key-renewed");
    assert!(!local_received.exists());
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
            denied = access_denied_reply(&bin).display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The recorded output, expiring far in the future so only the cache's
    // half-lifetime rule could ask for another call (it does not here).
    let mut recorded: serde_json::Value =
        serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
        mints: true,
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
        Ok(vec![CredentialSyncAction::AwsCredentialsPushed])
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
        error.contains("AccessDenied") && error.contains("trust policy"),
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
    let mut recorded: serde_json::Value =
        serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
                mints: true,
            });
            target
        })
        .collect::<Vec<_>>();

    let outcomes = reconcile_profile(&targets, None).await;

    assert_eq!(outcomes.len(), SESSIONS, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.outcome == Ok(vec![CredentialSyncAction::AwsCredentialsPushed])),
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
    // file to wait for: its call finishes only after that file exists. The
    // test's own deadline bounds a regression that runs sessions serially.
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        "#!/bin/sh\n\
         while [ -n \"$MJ_AWS_WAIT_FOR\" ] && [ ! -e \"$MJ_AWS_WAIT_FOR\" ]; do\n\
           /bin/sleep 0.05\n\
         done\n\
         exec /bin/cat \"$MJ_AWS_ASSUME_FIXTURE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut recorded: serde_json::Value =
        serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
            mints: true,
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

    let outcomes = tokio::time::timeout(Duration::from_secs(30), reconcile_profile(&targets, None))
        .await
        .expect("the fast session completed while the slow one was still minting");

    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.outcome == Ok(vec![CredentialSyncAction::AwsCredentialsPushed])),
        "{outcomes:?}"
    );
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
            access_denied_reply(&bin).display()
        ),
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
        let mut recorded: serde_json::Value =
            serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
    let reconcile_for = |aws| {
        credential_sync::run_on_session(
            &SessionRelays::Direct,
            &target,
            credential_sync::SyncWork::Aws(Box::new(aws)),
            credential_sync::JobHold::new(None, None),
        )
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
    let pass = mj_core::runtime::spawn({
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
    let mut recorded: serde_json::Value =
        serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
            .is_ok_and(|actions| actions.contains(&CredentialSyncAction::AwsCredentialsPushed))),
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

/// A Claude login as the sync sees it. A copy without `refresh` is what a
/// harness holds since #1490.
#[cfg(unix)]
fn claude_login(expires_at: i64, access: &str, refresh: Option<&str>) -> Vec<u8> {
    let mut oauth = serde_json::json!({
        "accessToken": access,
        "expiresAt": expires_at,
        "scopes": ["user:inference"],
    });
    if let Some(refresh) = refresh {
        oauth["refreshToken"] = refresh.into();
    }
    serde_json::to_vec(&serde_json::json!({ "claudeAiOauth": oauth })).unwrap()
}

/// [`codex_sync_target`] for a Claude profile whose session holds `login`,
/// which the fixture serves back and records pushes over at `received`.
#[cfg(unix)]
fn claude_sync_target(
    home: &std::path::Path,
    session_id: &str,
    login: &[u8],
    received: &std::path::Path,
) -> CredentialSyncTarget {
    let mut target = codex_sync_target(home, session_id, "2026-09-25T20:00:00.000Z", received);
    target.profile_id = "claude1".into();
    target.harness = mj_core::config::HarnessKind::Claude;
    std::fs::write(received, login).unwrap();
    let session = CredentialSnapshot::of(mj_core::config::HarnessKind::Claude, login);
    target.spec.args[4] = session.fingerprint;
    target.spec.args[5] = session.freshness_epoch_ms.unwrap().to_string();
    target
}

/// #1490: a session copy holds no refresh token, so the sync must converge a
/// copy that still holds the host's, never let a copy without one replace the
/// host, and adopt a copy that carries a newer login of its own, then hand it
/// back without the token.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn claude_sync_converges_copies_to_the_hosts_login_without_its_refresh_token() {
    let host_login = claude_login(2_000, "access-host", Some("refresh-host"));
    let stripped_host = mj_core::credentials::credential_copy_for_harness(
        mj_core::config::HarnessKind::Claude,
        &host_login,
    )
    .into_owned();
    // (session copy, expected pushes, expected pulls, host after)
    let own_login = claude_login(3_000, "access-own", Some("refresh-own"));
    let stripped_own = mj_core::credentials::credential_copy_for_harness(
        mj_core::config::HarnessKind::Claude,
        &own_login,
    )
    .into_owned();
    #[allow(clippy::type_complexity, reason = "one-off table of scenarios")]
    let cases: [(&str, Vec<u8>, Vec<CredentialSyncAction>, &[u8], &[u8]); 5] = [
        // (a) staged before the rule: the host's own bytes, token included.
        (
            "pre-upgrade copy",
            host_login.clone(),
            vec![CredentialSyncAction::Pushed],
            &host_login,
            &stripped_host,
        ),
        // Already converged.
        (
            "converged copy",
            stripped_host.clone(),
            vec![],
            &host_login,
            &stripped_host,
        ),
        // An older copy catches up.
        (
            "older copy",
            claude_login(1_000, "access-old", None),
            vec![CredentialSyncAction::Pushed],
            &host_login,
            &stripped_host,
        ),
        // (b) later expiry but no refresh token: neither side changes.
        (
            "later copy without a refresh token",
            claude_login(9_000, "access-later", None),
            vec![],
            &host_login,
            &claude_login(9_000, "access-later", None),
        ),
        // (c) an in-session login: adopted, then returned without the token.
        (
            "copy with its own newer login",
            own_login.clone(),
            // The pull makes a second pass; the fixture's worker keeps
            // reporting its first state, so that pass pushes again. The host
            // holding the session's login proves the pull.
            vec![CredentialSyncAction::Pushed],
            &own_login,
            &stripped_own,
        ),
    ];
    for (name, session_login, actions, host_after, session_after) in cases {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let received = scratch.path().join("session-login");
        let canonical = home.path().join(".credentials.json");
        std::fs::write(&canonical, &host_login).unwrap();
        let target = claude_sync_target(home.path(), "claude-session", &session_login, &received);

        let outcomes = reconcile_profile(&[target], Some("claude-session")).await;

        let outcome = outcomes
            .iter()
            .find(|o| o.session_id == "claude-session")
            .unwrap();
        assert_eq!(outcome.outcome, Ok(actions), "{name}: {outcomes:?}");
        assert_eq!(
            std::fs::read(&canonical).unwrap(),
            host_after,
            "{name}: host"
        );
        assert_eq!(
            std::fs::read(&received).unwrap(),
            session_after,
            "{name}: session"
        );
    }
}

/// One macOS sync pass for one session whose copy is `session_login`, against
/// a host whose login `keychain` holds, with `claude_dir` standing for
/// `~/.claude`. Returns the session's copy afterwards
/// and what the pass did.
#[cfg(unix)]
async fn mac_sync_pass(
    keychain: &Arc<crate::claude_login::fake::FakeKeychain>,
    claude_dir: &std::path::Path,
    session_login: &[u8],
) -> (Vec<u8>, credential_sync::SessionSync) {
    let kind = mj_core::config::HarnessKind::Claude;
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let received = scratch.path().join("session-login");
    let target = claude_sync_target(home.path(), "claude-session", session_login, &received);
    let skills = Arc::new(CanonicalSkills::collect(&target));
    let read = match crate::harness_auth::read_claude_keychain_login(&**keychain).unwrap() {
        crate::harness_auth::KeychainLogin::Present(bytes) => {
            (CredentialSnapshot::of(kind, &bytes), bytes)
        }
        _ => (CredentialSnapshot::absent(), Vec::new()),
    };
    let pass = credential_sync::PassLogin::Keychain {
        read: Arc::new(Ok(read)),
        access: credential_sync::LoginAccess::new(
            mj_core::config::HarnessLoginStore::MacKeychain,
            keychain.clone(),
        )
        .with_lock_marker(claude_dir.join(".credentials.json")),
    };
    let sync = reconcile_session(
        &SessionRelays::Direct,
        &target,
        &skills,
        &credential_sync::GithubSync::Known(None),
        credential_sync::HostLogins {
            slopcop: &credential_sync::HostSlopcopLogin::Unknown,
            harness: &pass,
            pinned: None,
        },
        credential_sync::JobHold::new(None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !home.path().join(".credentials.json").exists(),
        "the profile home gains no login file"
    );
    (std::fs::read(&received).unwrap(), sync)
}

/// #1491: a login the macOS Keychain holds reaches sessions as the stripped
/// copy. A session copy that is strictly newer and holds its own refresh token
/// (a pre-upgrade container or remote session that refreshed the shared grant,
/// so the spent token is the only one the host has left) is pulled into the
/// Keychain, with the secret on `security`'s stdin and never in its arguments,
/// and the session is handed the stripped copy back.
#[cfg(unix)]
// Hard-won: #1491 review: a macOS session's refreshed login stayed in the session while the Keychain kept the spent token
#[tokio::test]
async fn keychain_login_sync_pulls_a_newer_session_login_into_the_keychain_and_pushes_it_back_stripped()
 {
    let kind = mj_core::config::HarnessKind::Claude;
    let host_login = claude_login(2_000, "access-host", Some("refresh-host"));
    let stripped_host =
        mj_core::credentials::credential_copy_for_harness(kind, &host_login).into_owned();
    let own_login = claude_login(3_000, "access-own", Some("refresh-own"));
    let stripped_own =
        mj_core::credentials::credential_copy_for_harness(kind, &own_login).into_owned();
    let stripped_newer = claude_login(3_000, "access-stripped", None);
    let claude_dir = tempfile::tempdir().unwrap();
    // (name, session copy, expected actions, session after, Keychain after)
    #[allow(clippy::type_complexity, reason = "one-off table of scenarios")]
    let cases: [(&str, Vec<u8>, Vec<CredentialSyncAction>, Vec<u8>, Vec<u8>); 5] = [
        (
            "pre-upgrade copy that still holds the host's refresh token",
            host_login.clone(),
            vec![CredentialSyncAction::Pushed],
            stripped_host.clone(),
            host_login.clone(),
        ),
        (
            "converged copy",
            stripped_host.clone(),
            vec![],
            stripped_host.clone(),
            host_login.clone(),
        ),
        (
            "older copy",
            claude_login(1_000, "access-old", None),
            vec![CredentialSyncAction::Pushed],
            stripped_host.clone(),
            host_login.clone(),
        ),
        (
            "newer copy that holds its own refresh token",
            own_login.clone(),
            vec![CredentialSyncAction::Pulled, CredentialSyncAction::Pushed],
            stripped_own.clone(),
            own_login.clone(),
        ),
        (
            "newer copy without a refresh token cannot become the host's login",
            stripped_newer.clone(),
            vec![],
            stripped_newer.clone(),
            host_login.clone(),
        ),
    ];
    for (name, session_login, actions, session_after, keychain_after) in cases {
        let keychain = Arc::new(
            crate::claude_login::fake::FakeKeychain::holding(&host_login)
                .requiring_lock_in(claude_dir.path()),
        );

        let (session, sync) = mac_sync_pass(&keychain, claude_dir.path(), &session_login).await;

        assert!(sync.failures.is_empty(), "{name}: {:?}", sync.failures);
        assert_eq!(sync.actions, actions, "{name}");
        assert_eq!(session, session_after, "{name}: session");
        assert_eq!(
            keychain.login().unwrap(),
            keychain_after,
            "{name}: Keychain"
        );
        for (args, stdin) in keychain.calls() {
            let secret_in_arguments = args
                .iter()
                .any(|arg| arg.contains("refresh-own") || arg.contains("refresh-host"));
            assert!(!secret_in_arguments, "{name}: {args:?}");
            if args == ["-i"] {
                assert!(stdin.is_some(), "{name}: the write comes on stdin");
            }
        }
    }

    // A Keychain that refuses the write fails the pull as a login step. The
    // session keeps its login, and nothing is written to a file instead.
    let keychain = Arc::new(crate::claude_login::fake::FakeKeychain::holding(
        &host_login,
    ));
    keychain.refuse_writes();
    let (session, sync) = mac_sync_pass(&keychain, claude_dir.path(), &own_login).await;
    assert_eq!(sync.failures.len(), 1, "{:?}", sync.failures);
    assert_eq!(sync.failures[0].step, SyncStep::HarnessLogin);
    assert_eq!(session, own_login);
    assert_eq!(keychain.login().unwrap(), host_login);
    assert!(!claude_dir.path().join(".oauth_refresh.lock").exists());
}

#[cfg(unix)]
// Hard-won: #1485: a credential read could overwrite a host login refreshed while the relay request was in flight.
#[tokio::test]
async fn credential_sync_reconciles_newer_destinations_before_reporting_auth_recovery() {
    for push_race in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let old = "2026-09-25T20:00:00.000Z";
        let worker = "2026-09-25T21:00:00.000Z";
        let latest = "2026-09-25T22:00:00.000Z";
        let canonical = home.path().join("auth.json");
        let read_from = scratch.path().join("worker");
        let latest_file = scratch.path().join("latest");
        std::fs::write(
            &canonical,
            codex_login(if push_race { worker } else { old }),
        )
        .unwrap();
        std::fs::write(
            &read_from,
            codex_login(if push_race { old } else { worker }),
        )
        .unwrap();
        std::fs::write(&latest_file, codex_login(latest)).unwrap();
        let mut target = codex_sync_target(
            home.path(),
            "login-race",
            if push_race { old } else { worker },
            &read_from,
        );
        target.spec.args.extend([
            String::new(),
            if push_race {
                String::new()
            } else {
                canonical.to_string_lossy().into_owned()
            },
            if push_race {
                String::new()
            } else {
                latest_file.to_string_lossy().into_owned()
            },
            if push_race {
                latest_file.to_string_lossy().into_owned()
            } else {
                String::new()
            },
        ]);

        let outcomes = reconcile_profile(&[target], Some("login-race")).await;

        assert_eq!(std::fs::read(&canonical).unwrap(), codex_login(latest));
        assert_eq!(std::fs::read(&read_from).unwrap(), codex_login(latest));
        let outcome = outcomes
            .iter()
            .find(|outcome| outcome.session_id == "login-race")
            .unwrap();
        let expected = if push_race {
            CredentialSyncAction::Pulled
        } else {
            CredentialSyncAction::Pushed
        };
        assert_eq!(outcome.outcome, Ok(vec![expected]), "{outcomes:?}");
        let result = CredentialSyncResult {
            profile_id: "codex".into(),
            trigger: Some(mj_core::credentials::CredentialSyncCause {
                session_id: "login-race".into(),
                ordinal: 1,
                reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
                login_fingerprint: None,
            }),
            failure: None,
            partial: false,
            outcomes,
        };
        let profile = mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            home.path().to_owned(),
        );
        let mut rejected = mj_core::credentials::RejectedLogins::default();
        rejected.observe(&result, &profile);
        assert_eq!(rejected.refusal("codex"), None);
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn the_credential_cache_is_swept_on_the_schedule_even_with_no_sessions() {
    use mj_core::targets::{CommandExecutor, CommandOutput};
    struct ShortLived;
    impl CommandExecutor for ShortLived {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            let mut recorded: serde_json::Value =
                serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
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
        mints: true,
    };
    let cache = crate::aws_assume_role::AwsCredentialCache::shared();
    cache
        .credentials_for(
            "swept-last-session",
            &source,
            &ShortLived,
            Duration::from_secs(5),
        )
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
    std::fs::write(root.path().join("remove-fails"), "").unwrap();
    std::fs::write(
        root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    let withdrawing = aws_sync_session(home.path(), root.path(), &token, &refusing);
    let outcomes = reconcile_profile(std::slice::from_ref(&withdrawing), None).await;
    // The refused mint and the withdrawal that failed are both said, in one
    // failure for the step, the withdrawal first.
    let failure = outcomes[0].failure_text().unwrap();
    assert_eq!(outcomes[0].step_failures.len(), 1, "{failure}");
    let (withdrawal, mint) = (
        failure.find("AWS credentials withdrawal").expect(&failure),
        failure.find("AccessDenied").expect(&failure),
    );
    assert!(withdrawal < mint, "{failure}");
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

    let (_, result) = credential_sync::aws_cycle(
        &access!(),
        &target,
        credential_sync::AwsCycleOptions::default(),
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

/// Ends a stalled `aws` stub when it drops, even if the test failed first: the
/// mint has no deadline of its own, and a stub still waiting would keep the
/// test runtime from shutting down. (The stub also ends when its directory is
/// removed, in case it polls only after that.)
#[cfg(unix)]
struct ReleaseOnDrop(std::path::PathBuf);

#[cfg(unix)]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

// A triggered session's renewal can stall in STS or behind its actor. The
// trigger's answer waits for it, but the profile's slot does not: the profile's
// other syncs (login, GitHub token, skills) run meanwhile.
#[cfg(unix)]
#[tokio::test]
async fn a_stalled_triggered_renewal_does_not_hold_the_profiles_other_syncs() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat-1").unwrap();
    // An `aws` that does not answer until `release` exists.
    let cli = granting_aws_cli(scratch.path());
    let (started, release) = (
        scratch.path().join("sts-started"),
        scratch.path().join("sts-release"),
    );
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/usr/bin/touch {started}\nwhile [ ! -e {release} ] && [ -d {dir} ]; do /bin/sleep 0.05; done\nexec /bin/cat {output}\n",
            started = started.display(),
            release = release.display(),
            dir = scratch.path().display(),
            output = scratch.path().join("granted.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _release_on_exit = ReleaseOnDrop(release.clone());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "stalled", None);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let wait_for = async |what: &str, condition: &(dyn Fn() -> bool + Sync)| {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    };
    let github = root.join("github");
    // The profile's first pass ran, and the renewal is stuck in STS.
    wait_for("the first pass", &|| github.exists()).await;
    wait_for("the STS call", &|| started.exists()).await;

    // An authentication failure in the session wakes its renewal. The pass
    // for the trigger pushes the new token and ends; the renewal has not.
    std::fs::write(&token, "pat-2").unwrap();
    handle.sync_profile_now(
        &target.profile_id,
        Some(mj_core::credentials::CredentialSyncCause {
            session_id: "stalled".into(),
            ordinal: 1,
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            login_fingerprint: None,
        }),
    );
    wait_for("the trigger's pass", &|| {
        std::fs::read_to_string(&github).is_ok_and(|text| text == "pat-2")
    })
    .await;

    // A second sync of the same profile runs while the renewal still waits.
    std::fs::write(&token, "pat-3").unwrap();
    handle.sync_profile_now(&target.profile_id, None);
    wait_for("the second sync", &|| {
        std::fs::read_to_string(&github).is_ok_and(|text| text == "pat-3")
    })
    .await;
    assert!(!root.join("aws").exists(), "the stalled renewal finished");
    while let Some(result) = coordinator.try_result() {
        assert!(
            result.trigger.is_none(),
            "the trigger was answered before its renewal ended: {result:?}"
        );
    }

    // The renewal ends; the trigger's answer arrives, with the renewal in it.
    std::fs::write(&release, "").unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.trigger.is_some() {
                return result;
            }
        }
    })
    .await
    .expect("the trigger's answer arrived once the renewal ended");
    // The stalled cycle installed the credentials and reported that itself;
    // the woken cycle after it found them current. Both ended without failure.
    assert!(root.join("aws").exists());
    assert!(answered.failures().next().is_none(), "{answered:?}");
    assert!(
        answered
            .outcomes
            .iter()
            .any(|outcome| outcome.session_id == "stalled"),
        "{answered:?}"
    );
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
    let steps = credential_sync::SessionSteps::default();
    let queue = || async {
        let work =
            credential_sync::SyncWork::Aws(Box::new(credential_sync::AwsSync::Unavailable {
                error: "no credentials".into(),
                scoped_required: true,
                expected_scope: None,
            }));
        let hold = credential_sync::JobHold::new(Some(steps.enter(&session).await), None);
        credential_sync::CredentialSyncJob::queue(target.clone(), work, hold)
    };

    // Queued: the awaiting task stops, the hold is released at once, and the
    // actor, when it gets to the job, drops it.
    let (job, response, queued) = queue().await;
    drop((response, queued));
    tokio::time::timeout(Duration::from_secs(5), steps.enter(&session))
        .await
        .expect("the install lock was not released with the abandoned job");
    let mut client = RelayClient::connect(&target.spec, &session).await.unwrap();
    Box::new(job).run(&mut client).await;
    assert!(root.join("aws").exists(), "an abandoned job ran");
    drop(client);

    // Running: the awaiting task stops while the actor talks to the worker.
    // The job keeps the session until it ends.
    let (job, response, queued) = queue().await;
    let mut client = RelayClient::connect(&target.spec, &session).await.unwrap();
    let running = mj_core::runtime::spawn(async move { Box::new(job).run(&mut client).await });
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.join("waiting").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the job reached the worker");
    drop((response, queued));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), steps.enter(&session))
            .await
            .is_err(),
        "a running job lost its install lock"
    );
    std::fs::write(&release, "").unwrap();
    running.await.unwrap();
    assert!(
        !root.join("aws").exists(),
        "the running job did not finish its work"
    );
}

// A renewal stopped (its target changed) before it answered a trigger leaves
// the renewal's outcome unknown, and the trigger's answer says so.
#[cfg(unix)]
#[tokio::test]
async fn a_trigger_whose_renewal_is_stopped_reports_the_outcome_as_unknown() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let (started, release) = (
        scratch.path().join("sts-started"),
        scratch.path().join("sts-release"),
    );
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/usr/bin/touch {started}\nwhile [ ! -e {release} ] && [ -d {dir} ]; do /bin/sleep 0.05; done\nexec /bin/cat {output}\n",
            started = started.display(),
            release = release.display(),
            dir = scratch.path().display(),
            output = scratch.path().join("granted.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _release_on_exit = ReleaseOnDrop(release.clone());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "replaced", None);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let github = root.join("github");
    let wait_for = async |what: &str, condition: &(dyn Fn() -> bool + Sync)| {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    };
    wait_for("the first pass", &|| github.exists()).await;
    wait_for("the STS call", &|| started.exists()).await;
    // The trigger's pass runs (so its wake has been sent to the renewal, which
    // is stuck in STS).
    std::fs::write(&token, "pat-2").unwrap();
    handle.sync_profile_now(
        &target.profile_id,
        Some(mj_core::credentials::CredentialSyncCause {
            session_id: "replaced".into(),
            ordinal: 1,
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            login_fingerprint: None,
        }),
    );
    wait_for("the trigger's pass", &|| {
        std::fs::read_to_string(&github).is_ok_and(|text| text == "pat-2")
    })
    .await;

    // The role is edited: the stalled renewal's task is replaced, and the
    // wake it never answered is dropped with it.
    let mut changed = target.clone();
    changed
        .aws_credentials
        .as_mut()
        .unwrap()
        .environment
        .insert("AWS_REGION".into(), "us-east-2".into());
    handle.set_targets(vec![changed]);
    let answered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = coordinator.result().await.expect("coordinator stopped");
            if result.trigger.is_some() {
                return result;
            }
        }
    })
    .await
    .expect("the trigger was answered");
    let failure = answered
        .failures()
        .map(|(_, _, detail)| detail.to_owned())
        .collect::<Vec<_>>()
        .join("; ");
    assert!(failure.contains("is unknown"), "{answered:?}");
}

// A mint that has to call AWS can take as long as it takes, so credentials that
// do not match a newly configured role are withdrawn before it starts, not
// after it returns, and the step says the replacement is pending.
#[cfg(unix)]
#[tokio::test]
async fn obsolete_credentials_are_withdrawn_before_a_stuck_mint_and_the_step_says_so() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let (started, release) = (
        scratch.path().join("sts-started"),
        scratch.path().join("sts-release"),
    );
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/usr/bin/touch {started}\nwhile [ ! -e {release} ] && [ -d {dir} ]; do /bin/sleep 0.05; done\nexec /bin/cat {output}\n",
            started = started.display(),
            release = release.display(),
            dir = scratch.path().display(),
            output = scratch.path().join("granted.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _release_on_exit = ReleaseOnDrop(release.clone());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    // A running session holding the host's own credentials, from before the
    // role was configured.
    std::fs::write(root.join("aws"), br#"{"Version":1,"AccessKeyId":"AKIA"}"#).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "obsolete", None);
    super::credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let (results, mut results_rx) = mpsc::unbounded_channel();
    let env = credential_sync::RenewalEnv {
        relays: SessionRelays::Direct,
        runtime: None,
        installs: Default::default(),
        results,
        retry: Duration::from_millis(100),
    };
    let (_wakes, wake_rx) = mpsc::unbounded_channel();
    let renewal = mj_core::runtime::spawn(credential_sync::renew_session_aws(env, target, wake_rx));

    // The credentials are withdrawn and the step says why, while STS has not
    // answered (the mint is still waiting on it).
    let interim = tokio::time::timeout(Duration::from_secs(20), results_rx.recv())
        .await
        .expect("the withdrawal was reported before the mint returned")
        .unwrap();
    assert!(!root.join("aws").exists(), "obsolete credentials were kept");
    assert!(!release.exists(), "the mint had been released");
    assert!(interim.partial);
    let outcome = &interim.outcomes[0];
    assert_eq!(
        outcome.outcome,
        Ok(vec![CredentialSyncAction::AwsCredentialsRemoved])
    );
    let failure = outcome.failure_text().unwrap();
    assert!(
        failure.contains("withdrawn") && failure.contains("until new role-scoped"),
        "{failure}"
    );

    // The mint returns: the new credentials are installed and the failure ends.
    std::fs::write(&release, "").unwrap();
    let mut seen = vec![interim.clone()];
    let done = loop {
        let result = tokio::time::timeout(Duration::from_secs(20), results_rx.recv())
            .await
            .expect("the cycle ended")
            .unwrap();
        seen.push(result.clone());
        if result.failures().next().is_none() {
            break result;
        }
    };
    assert!(aws_pushed(&done, "obsolete"), "{done:?}");
    // The withdrawal is reported once, by the interim result, not again in the
    // cycle's own.
    let removals =
        seen.iter()
            .flat_map(|result| &result.outcomes)
            .filter(|outcome| {
                outcome.outcome.as_ref().is_ok_and(|actions| {
                    actions.contains(&CredentialSyncAction::AwsCredentialsRemoved)
                })
            })
            .count();
    assert_eq!(removals, 1, "{seen:?}");
    assert!(root.join("aws").exists());
    renewal.abort();
}

// A mint belongs to the renewal task that awaits it. When the task is
// superseded (its target changed, though not the role or the environment the
// cache is keyed by), the AWS CLI it started is terminated, so the cache
// entry's lock is released and the replacement is not left waiting on a helper
// nobody wants.
// Hard-won: 42c9f2103: the renewal teardown test assumed /proc on macOS.
#[cfg(unix)]
#[tokio::test]
async fn unscoped_aws_helper_cancels_without_holding_up_profile_sync() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let exported = scratch.path().join("exported.json");
    std::fs::write(
        &exported,
        br#"{"Version":1,"AccessKeyId":"key","SecretAccessKey":"secret"}"#,
    )
    .unwrap();
    let (pids, release) = (
        scratch.path().join("helper-pids"),
        scratch.path().join("helper-release"),
    );
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/bin/echo $$ >> {pids}\nwhile [ ! -e {release} ] && [ -d {dir} ]; do /bin/sleep 0.05; done\nexec /bin/cat {output}\n",
            pids = pids.display(),
            release = release.display(),
            dir = scratch.path().display(),
            output = exported.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _release_on_exit = ReleaseOnDrop(release.clone());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let mut target = aws_sync_session_named(home.path(), &root, &token, &cli, "superseded", None);
    target.aws_credentials.as_mut().unwrap().role = None;
    target.harness = mj_core::config::HarnessKind::Codex;
    target.skips_login_file_sync = false;
    let login = codex_login("2026-09-25T20:00:00Z");
    std::fs::write(home.path().join("auth.json"), &login).unwrap();
    std::fs::write(root.join("slopcop"), b"old login").unwrap();
    let coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let helper_pids = || -> Vec<u32> {
        std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    };
    let wait_for = async |what: &str, condition: &(dyn Fn() -> bool + Sync)| {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    };
    wait_for("the first helper", &|| helper_pids().len() == 1).await;
    let pass = tokio::time::timeout(
        Duration::from_secs(5),
        credential_sync::reconcile_profile_guarded(
            &access!(),
            std::slice::from_ref(&target),
            None,
            || Ok(None),
        ),
    )
    .await
    .expect("profile pass completed while AWS was blocked");
    assert!(
        pass.iter().all(|outcome| outcome.step_failures.is_empty()),
        "{pass:?}"
    );
    assert_eq!(std::fs::read(root.join("login")).unwrap(), login);
    assert!(root.join("skills").exists());
    assert_eq!(std::fs::read(root.join("github")).unwrap(), b"pat");
    assert!(!root.join("slopcop").exists());
    let stuck = helper_pids()[0];
    let birth = mj_core::subprocess::process_birth_identity(stuck)
        .expect("the first helper is still running");

    // The target changes without changing what the cache is keyed by: the
    // task is replaced, and its helper is terminated with it.
    let mut changed = target.clone();
    changed.skips_login_file_sync = !changed.skips_login_file_sync;
    handle.set_targets(vec![changed]);
    wait_for("the replacement's helper", &|| helper_pids().len() == 2).await;
    wait_for("the superseded helper to end", &|| {
        !mj_core::subprocess::process_birth_identity(stuck).is_ok_and(|current| current == birth)
    })
    .await;
    assert!(!root.join("aws").exists());

    // The replacement mints once the helper answers.
    std::fs::write(&release, "").unwrap();
    wait_for("the replacement's credentials", &|| {
        root.join("aws").exists()
    })
    .await;
    drop(coordinator);
}

// A worker-side edit cannot change the installed profile or the mint identity.
#[cfg(unix)]
#[tokio::test]
async fn a_session_that_edits_its_aws_configuration_still_gets_only_role_scoped_credentials() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let args = scratch.path().join("assume-role-args");
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/bin/echo \"$@\" >> {args}\nexec /bin/cat {output}\n",
            args = args.display(),
            output = scratch.path().join("granted.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let mut target = aws_sync_session_named(home.path(), &root, &token, &cli, "edited", None);
    target
        .aws_credentials
        .as_mut()
        .unwrap()
        .environment
        .insert("AWS_PROFILE".into(), "team".into());
    // The session renamed the profile section in its own AWS configuration,
    // hoping the sync would act for a host profile it picked.
    std::fs::write(
        root.join("aws-config"),
        "[profile victim]\ncredential_process = cat /nonexistent\n",
    )
    .unwrap();

    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.step_failures.is_empty()),
        "{outcomes:?}"
    );
    // The mint ran as the host's selection, never as a name from the session.
    let minted_as = std::fs::read_to_string(&args).unwrap();
    assert!(minted_as.contains("--profile team"), "{minted_as}");
    assert!(!minted_as.contains("victim"), "{minted_as}");
    // Renewal installs under the originally staged name.
    let installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("aws")).unwrap()).unwrap();
    assert_eq!(installed["Profile"], "team");
    assert!(
        std::fs::read_to_string(root.join("aws-scope.json"))
            .unwrap()
            .contains("arn:aws:iam::123456789012:role/mj-bedrock")
    );
}

// A session can delete or damage its own AWS configuration, so withdrawing the
// credentials that do not match the role must not wait on reading the profile
// name from it. The withdrawal comes first and a mint failure is reported.
#[cfg(unix)]
#[tokio::test]
async fn a_session_that_breaks_its_aws_configuration_still_has_host_credentials_withdrawn() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let started = scratch.path().join("sts-started");
    std::fs::write(
        cli.join("aws"),
        format!("#!/bin/sh\n/usr/bin/touch {}\nexit 1\n", started.display()),
    )
    .unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "broken", None);
    let host_credentials = br#"{"Version":1,"AccessKeyId":"AKIA"}"#;

    for (case, config) in [("deleted", None), ("invalid header", Some("[profile ]\n"))] {
        std::fs::write(root.join("aws"), host_credentials).unwrap();
        match config {
            Some(text) => std::fs::write(root.join("aws-config"), text).unwrap(),
            None => std::fs::remove_file(root.join("aws-config")).unwrap(),
        }

        let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

        assert!(
            !root.join("aws").exists(),
            "{case}: host credentials stayed"
        );
        let failure = outcomes[0].failure_text().unwrap();
        assert!(failure.contains("AWS credentials"), "{case}: {failure}");
        assert!(
            outcomes[0]
                .outcome
                .as_ref()
                .is_ok_and(|actions| actions.contains(&CredentialSyncAction::AwsCredentialsRemoved)),
            "{case}: {outcomes:?}"
        );
        assert!(started.exists(), "{case}: AWS CLI was not invoked");
    }
}

// A role configured for a profile means no session of that profile keeps
// credentials that do not match the role, whether or not the profile still
// authenticates with the AWS credential chain: that decides only whether
// anything is minted.
#[cfg(unix)]
#[tokio::test]
async fn a_role_on_a_profile_that_is_not_bedrock_still_withdraws_host_credentials() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let token = root.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    // Minting would be refused with AccessDenied; it must not be tried.
    let cli = refusing_aws_cli(root.path());
    std::fs::write(
        root.path().join("aws"),
        br#"{"Version":1,"AccessKeyId":"AKIA"}"#,
    )
    .unwrap();
    let mut target = aws_sync_session(home.path(), root.path(), &token, &cli);
    target.aws_credentials.as_mut().unwrap().mints = false;

    let outcomes = reconcile_profile(std::slice::from_ref(&target), None).await;

    assert!(!root.path().join("aws").exists(), "{outcomes:?}");
    assert!(
        outcomes[0]
            .outcome
            .as_ref()
            .is_ok_and(|actions| actions.contains(&CredentialSyncAction::AwsCredentialsRemoved)),
        "{outcomes:?}"
    );
    assert!(outcomes[0].step_failures.is_empty(), "{outcomes:?}");
}

// A renewal task that ends on its own (it panics) is noticed at the next sync,
// reported as the session's failure, and replaced; a trigger that wakes it
// meanwhile is told the renewal's outcome is unknown, not left without an
// answer.
#[cfg(unix)]
#[tokio::test]
async fn a_renewal_task_that_panics_is_reported_and_replaced_at_the_next_sync() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "panicking", None);
    let (results, mut results_rx) = mpsc::unbounded_channel();
    let env = credential_sync::RenewalEnv {
        relays: SessionRelays::Direct,
        runtime: None,
        installs: Default::default(),
        results,
        retry: Duration::from_millis(100),
    };
    // The session's task, which dies as soon as it runs and drops what it
    // listened on.
    let (wakes, wake_rx) = mpsc::unbounded_channel();
    let dying = mj_core::runtime::spawn(async move {
        let _wakes = wake_rx;
        panic!("renewal task panicked on purpose");
    });
    let mut tasks = credential_sync::RenewalTasks::with_task(&target, dying, wakes);
    while tasks.is_running("panicking") {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A trigger arrives before the next sync: it has no live task to answer it.
    let answer = tasks.wake("panicking").expect("the session has a task");
    assert!(answer.await.is_err(), "a dead task answered");

    tasks.sync(std::slice::from_ref(&target), &env);

    let reported = tokio::time::timeout(Duration::from_secs(5), results_rx.recv())
        .await
        .expect("the stopped task was reported")
        .unwrap();
    assert!(reported.partial);
    let failure = reported.outcomes[0].failure_text().unwrap();
    assert!(
        failure.contains("renewal task stopped") && failure.contains("panicked"),
        "{failure}"
    );
    assert_eq!(reported.outcomes[0].step_failures[0].step, SyncStep::Aws);
    // Replaced: the new task runs its cycle and installs the credentials.
    assert!(tasks.is_running("panicking"));
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.join("aws").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the replacement task renewed the credentials");
}

// Failed renewals back off, doubling from the retry interval up to the
// credentials' half-life, so a permanent failure does not call STS and the
// worker once a minute for the life of the session.
#[test]
fn renewal_retries_double_up_to_the_credential_half_life() {
    let minute = Duration::from_secs(60);
    let role = role_config();
    let limit = crate::aws_assume_role::credential_half_life(&role);
    assert_eq!(limit, Duration::from_secs(1800), "AWS's one-hour default");
    let delays = (0..8)
        .map(|failures| credential_sync::retry_delay(minute, failures, limit, None).as_secs())
        .collect::<Vec<_>>();
    assert_eq!(delays, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
    // A configured lifetime sets the limit; a limit under the base is the base.
    let short = mj_core::config::AwsRoleConfig {
        duration_seconds: Some(900),
        ..role
    };
    assert_eq!(
        crate::aws_assume_role::credential_half_life(&short),
        Duration::from_secs(450)
    );
    assert_eq!(
        credential_sync::retry_delay(minute, 5, Duration::ZERO, None),
        minute
    );
    // While the credentials are valid, the delay never reaches past half of
    // what they have left. One-hour credentials whose renewals fail from the
    // half-life on (minutes 30, 31, 33, 37 and 45) would otherwise next be
    // tried at minute 61, after they expired at 60.
    let expiry = Duration::from_secs(3600);
    let mut attempts = vec![Duration::from_secs(30 * 60)];
    for failures in 0.. {
        let now = *attempts.last().unwrap();
        let remaining = expiry.checked_sub(now).filter(|left| !left.is_zero());
        let next = now + credential_sync::retry_delay(minute, failures, limit, remaining);
        if next >= expiry {
            break;
        }
        attempts.push(next);
    }
    let minutes = attempts
        .iter()
        .take(6)
        .map(|attempt| attempt.as_secs_f64() / 60.0)
        .collect::<Vec<_>>();
    assert_eq!(minutes[..5], [30.0, 31.0, 33.0, 37.0, 45.0], "{minutes:?}");
    // The attempt after minute 45 lands before the credentials expire, and
    // the attempts keep closing in, a minute apart at the end, until they do.
    assert!(
        attempts[5] > attempts[4] && attempts[5] < expiry,
        "{attempts:?}"
    );
    assert!(
        *attempts.last().unwrap() + minute >= expiry,
        "attempts stopped well before the expiry: {attempts:?}"
    );
    // Once they have expired, the backoff stands.
    assert_eq!(
        credential_sync::retry_delay(minute, 5, limit, None),
        Duration::from_secs(1800)
    );

    // Any number of failures stays within the limit.
    assert_eq!(
        credential_sync::retry_delay(minute, u32::MAX, limit, None),
        limit
    );
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
            if result.partial && aws_pushed(&result, "auth-session") {
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
            ordinal: 1,
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            login_fingerprint: None,
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
    assert!(aws_pushed(&triggered, "auth-session"), "{triggered:?}");
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
    let waiting = mj_core::runtime::spawn({
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
            unchanged_login: None,
            current_access_token: None,
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
    // A connection error is the session's, not the harness login's: it is
    // reported without a step.
    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![merged.clone()],
        partial: false,
    };
    assert_eq!(
        result.failures().collect::<Vec<_>>(),
        [("session", None, "worker proxy disconnected")]
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
    // A role-scoped renewal needs the worker's staged profile, which a worker
    // before protocol 55 cannot report, so nothing is minted and the one
    // failure says so; the old worker cannot withdraw either.
    assert_eq!(outcomes[0].step_failures.len(), 1, "{failure}");
    assert!(
        failure.contains("predates withdrawing AWS credentials"),
        "{failure}"
    );
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
            ordinal: 1,
            reason: mj_core::credentials::CredentialSyncReason::AuthenticationFailure,
            login_fingerprint: None,
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
            unchanged_login: Some(CredentialSnapshot::of(
                mj_core::config::HarnessKind::Codex,
                &codex_login("2026-09-25T20:00:00.000Z"),
            )),
            current_access_token: None,
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
                unchanged_login: None,
                current_access_token: None,
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

#[cfg(unix)]
#[tokio::test]
async fn command_retention_skips_protocol_72_without_writing_a_request() {
    let script = format!(
        r#"python3 -c '
import json, sys
req = json.loads(sys.stdin.readline())
print(json.dumps({{"request_id":req["request_id"], "protocol_version":72, "result":"ok", "payload":{{"type":"hello", "data":{{"negotiated":72, "relay_version":"retention-old", "session_id":{session:?}}}}}}}), flush=True)
sys.stdin.read()
'"#,
        session = SESSION_ID
    );
    let spec = CommandSpec::new("sh", ["-c", &script]).purpose("old retention relay fixture");
    let mut client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .unwrap();
    let before = client.next_request;
    assert_eq!(client.command_retention_status().await.unwrap(), None);
    client
        .update_command_retention(
            Some("move-operation".into()),
            ["queued-config".into()].into(),
            false,
        )
        .await
        .unwrap();
    client
        .update_command_retention(None, Default::default(), true)
        .await
        .unwrap();
    let error = client.live_session_baseline().await.unwrap_err();
    assert!(error.is::<LiveBaselineUnsupported>(), "{error:#}");
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
    for protocol in [43, 44, 58] {
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

#[cfg(unix)]
// Hard-won: #1500: a single project-memory file could exceed the relay frame on download or upload.
#[tokio::test]
async fn project_memory_transfer_pages_large_files_and_preserves_cas_and_cleanup() {
    use mj_core::project_memory::{ProjectMemorySnapshot, ReplicaReplaceOutcome, TreeVersion};
    let script = r#"
import base64, gzip, json, pathlib, sys, time
mode, size, marker = sys.argv[1], int(sys.argv[2]), pathlib.Path(sys.argv[3])
alphabet = b'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
state, content = 7, bytearray(size)
for i in range(size):
    state = (state * 1664525 + 1013904223) & 0xffffffff
    content[i] = alphabet[state >> 26]
tree = {'files': {'/one-huge.md': content.decode()}}
current = {'files': dict(tree['files'], **{'/delete.md': 'remove this'})}
active, ident, injected = None, 0, False
limit = 8 * 1024 * 1024
for line in sys.stdin:
    assert len(line.encode()) <= limit
    req = json.loads(line)
    method = req['request']['method']
    if method == 'hello':
        payload = {'type':'hello','data':{'negotiated':59,'relay_version':'memory-pages-peer','session_id':sys.argv[4]}}
    else:
        assert method == 'project_memory_transfer', 'new protocol must never use legacy requests'
        action = req['request']['params']['action']
        name, params = action['action'], action.get('params', {})
        error = None
        if name == 'begin_snapshot':
            assert active is None
            ident += 1
            wire = {'gzip': base64.b64encode(gzip.compress(json.dumps(current['files']).encode())).decode()}
            data = json.dumps({'baseline':wire,'replica':wire}).encode()
            if mode == 'healthy': assert len(data) > limit
            active = ('download', data)
            result = {'action':'snapshot_started','data':{'transfer_id':ident,'total_bytes':len(data)}}
        elif name == 'read_snapshot_page':
            assert params['transfer_id'] == ident and active[0] == 'download'
            offset = params['offset']
            # Exact serialized response overhead plus newline determines the base64 capacity.
            template = {'request_id':req['request_id'],'protocol_version':59,'result':'ok','payload':{'type':'project_memory_transfer','data':{'result':{'action':'snapshot_page','data':{'transfer_id':ident,'offset':offset,'next_offset':len(active[1]),'complete':False,'chunk':''}}}}}
            capacity = 3 * ((limit - len(json.dumps(template).encode()) - 1) // 4)
            chunk = active[1][offset:offset+capacity]
            end = offset + len(chunk)
            complete = end == len(active[1])
            result = {'action':'snapshot_page','data':{'transfer_id':ident,'offset':offset,'next_offset':end,'complete':complete,'chunk':base64.b64encode(chunk).decode()}}
            if complete: active = None
        elif name == 'begin_upload':
            assert active is None
            ident += 1
            if mode == 'healthy': assert params['total_bytes'] > limit
            active = ('upload', bytearray(), params)
            result = {'action':'upload_started','data':{'transfer_id':ident}}
        elif name == 'append_upload':
            assert params['transfer_id'] == ident and active[0] == 'upload'
            assert params['offset'] == len(active[1])
            if mode == 'cancel':
                marker.write_text('append accepted')
                sys.stdin.read()
                sys.exit(0)
            if mode in ('append_error', 'abort_error') and not injected:
                injected = True
                error = 'injected append refusal'
            else:
                active[1].extend(base64.b64decode(params['chunk']))
                result = {'action':'upload_progress','data':{'transfer_id':ident,'next_offset':len(active[1])}}
        elif name == 'commit_upload':
            assert params['transfer_id'] == ident and active[0] == 'upload'
            assert len(active[1]) == active[2]['total_bytes']
            wire = json.loads(active[1])
            incoming = {'files':json.loads(gzip.decompress(base64.b64decode(wire['gzip'])))}
            assert incoming == tree
            kind = active[2]['kind']
            if kind['kind'] == 'install_snapshot':
                current['files'].update(incoming['files'])
                result = {'action':'installed'}
            else:
                assert kind['kind'] == 'replace_tree'
                replaced = kind['params']['expected_replica'] == 'current'
                if replaced: current = incoming
                result = {'action':'replaced','data':{'outcome':'replaced' if replaced else 'replica_changed'}}
            active = None
        elif name == 'abort':
            assert active is not None and params['transfer_id'] == ident
            if mode == 'abort_error': error = 'injected abort refusal'
            else:
                active = None
                result = {'action':'aborted','data':{'transfer_id':ident}}
        else: raise AssertionError(name)
        if error:
            response = {'request_id':req['request_id'],'protocol_version':59,'result':'error','error':{'code':'invalid_state','message':error,'retryable':False}}
            print(json.dumps(response), flush=True)
            continue
        payload = {'type':'project_memory_transfer','data':{'result':result}}
    response = json.dumps({'request_id':req['request_id'],'protocol_version':59,'result':'ok','payload':payload})
    assert len(response.encode()) + 1 <= limit
    print(response, flush=True)
"#;
    for mode in ["healthy", "append_error", "abort_error", "cancel"] {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("append");
        let size = if mode == "healthy" {
            12 * 1024 * 1024
        } else {
            128 * 1024
        };
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut state = 7u32;
        let content: String = (0..size)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                alphabet[(state >> 26) as usize] as char
            })
            .collect();
        let tree = ProjectMemorySnapshot {
            files: BTreeMap::from([("/one-huge.md".into(), content)]),
        };
        let spec = CommandSpec::new(
            "python3",
            [
                "-c".to_owned(),
                script.to_owned(),
                mode.to_owned(),
                size.to_string(),
                marker.to_string_lossy().into_owned(),
                SESSION_ID.to_owned(),
            ],
        )
        .purpose("paged project-memory protocol peer");
        let mut client =
            RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(30))
                .await
                .unwrap();
        if mode == "healthy" {
            let mut before = tree.clone();
            before
                .files
                .insert("/delete.md".into(), "remove this".into());
            assert_eq!(
                client.project_memory_snapshot().await.unwrap(),
                (before.clone(), before.clone())
            );
            client
                .install_project_memory_snapshot(tree.clone())
                .await
                .unwrap();
            assert_eq!(
                client.project_memory_snapshot().await.unwrap(),
                (before.clone(), before)
            );
            assert_eq!(
                client
                    .replace_project_memory_tree(TreeVersion("stale".into()), tree.clone())
                    .await
                    .unwrap(),
                ReplicaReplaceOutcome::ReplicaChanged
            );
            assert_eq!(
                client
                    .replace_project_memory_tree(TreeVersion("current".into()), tree.clone())
                    .await
                    .unwrap(),
                ReplicaReplaceOutcome::Replaced
            );
            assert_eq!(
                client.project_memory_snapshot().await.unwrap(),
                (tree.clone(), tree)
            );
        } else if mode == "cancel" {
            {
                let upload =
                    client.replace_project_memory_tree(TreeVersion("current".into()), tree);
                tokio::pin!(upload);
                tokio::select! {
                    result = &mut upload => panic!("upload completed before cancellation: {result:?}"),
                    () = async { while !marker.exists() { tokio::time::sleep(Duration::from_millis(10)).await; } } => {}
                }
            }
            assert!(client.abandoned.is_some());
            assert!(client.input.is_none());
        } else {
            let error = client
                .replace_project_memory_tree(TreeVersion("current".into()), tree.clone())
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("injected append refusal"));
            if mode == "abort_error" {
                assert!(format!("{error:#}").contains("injected abort refusal"));
                assert!(client.abandoned.is_some());
                assert!(client.input.is_none());
            } else {
                assert!(client.abandoned.is_none());
                assert_eq!(
                    client
                        .replace_project_memory_tree(TreeVersion("current".into()), tree)
                        .await
                        .unwrap(),
                    ReplicaReplaceOutcome::Replaced
                );
            }
        }
    }
}

// Hard-won: 0d930fcd3: a pull-only delivery had no canonical handout acknowledgment for fencing.
#[tokio::test]
async fn pull_only_turn_sync_absorbs_handout_before_delivery() {
    use mj_core::project_memory::{
        ProjectMemorySnapshot, SwapOutcome, commit_canonical, read_canonical_state,
    };
    use mj_core::relay::{
        ProjectMemoryDeliveryOutcome, ProjectMemoryTransferAction as Action,
        ProjectMemoryTransferResult as Reply, ProjectMemoryTurnSnapshot, ProjectMemoryWireTree,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("canonical/memory");
    let base = ProjectMemorySnapshot {
        files: [("/note.md".into(), "old".into())].into(),
    };
    let canonical = ProjectMemorySnapshot {
        files: [("/note.md".into(), "foreign".into())].into(),
    };
    let state = read_canonical_state(&root).unwrap();
    assert_eq!(
        commit_canonical(&root, &state, "other", &canonical, None).unwrap(),
        SwapOutcome::Swapped
    );
    let wire = ProjectMemoryTurnSnapshot {
        base: ProjectMemoryWireTree::encode(base.clone(), RELAY_PROTOCOL_VERSION).unwrap(),
        id: "pull-handout".into(),
        tree: ProjectMemoryWireTree::encode(base, RELAY_PROTOCOL_VERSION).unwrap(),
    };
    let bytes = serde_json::to_vec(&wire).unwrap();
    let (controller, worker) = tokio::io::duplex(16384);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let peer_root = root.clone();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut lines = BufReader::new(reader).lines();
        for expected in 0..5 {
            let line = lines.next_line().await.unwrap().unwrap();
            let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            let RelayRequest::ProjectMemoryTransfer { action } = request.request else {
                panic!("unexpected request")
            };
            let reply = match (expected, action) {
                (0, Action::BeginTurnSnapshot { .. }) => Reply::SnapshotStarted {
                    transfer_id: 1,
                    total_bytes: bytes.len() as u64,
                },
                (
                    1,
                    Action::ReadSnapshotPage {
                        transfer_id: 1,
                        offset: 0,
                    },
                ) => Reply::SnapshotPage {
                    transfer_id: 1,
                    offset: 0,
                    next_offset: bytes.len() as u64,
                    complete: true,
                    chunk: BASE64.encode(&bytes),
                },
                (2, Action::BeginUpload { .. }) => {
                    let state = read_canonical_state(&peer_root).unwrap();
                    assert_eq!(
                        state.absorbed.get(SESSION_ID).map(String::as_str),
                        Some("pull-handout")
                    );
                    Reply::UploadStarted { transfer_id: 2 }
                }
                (
                    3,
                    Action::AppendUpload {
                        transfer_id: 2,
                        offset,
                        chunk,
                    },
                ) => Reply::UploadProgress {
                    transfer_id: 2,
                    next_offset: offset + BASE64.decode(chunk).unwrap().len() as u64,
                },
                (4, Action::CommitUpload { transfer_id: 2 }) => Reply::Delivered {
                    outcome: ProjectMemoryDeliveryOutcome::Delivered,
                },
                other => panic!("unexpected action: {other:?}"),
            };
            let response = RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: RELAY_PROTOCOL_VERSION,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ProjectMemoryTransfer { result: reply },
                },
            };
            let mut frame = serde_json::to_vec(&response).unwrap();
            frame.push(b'\n');
            writer.write_all(&frame).await.unwrap();
        }
    });
    let target = crate::session_manager::ProjectMemorySyncTarget {
        canonical_root: root,
        relay: CommandSpec::new("unused", ["unused"]),
    };
    crate::session_manager::sync_turn_project_memory(
        &mut client,
        &target,
        SESSION_ID,
        &mj_core::config::Config::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    peer.await.unwrap();
}

// Hard-won: 0d930fcd3: a Busy reply does not retire the handout and cannot fence a delivery.
#[tokio::test]
async fn project_memory_fence_refuses_busy_snapshot() {
    use mj_core::relay::{
        ProjectMemoryTransferAction as Action, ProjectMemoryTransferResult as Reply,
    };
    let (controller, worker) = tokio::io::duplex(8192);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut lines = BufReader::new(reader).lines();
        let request: RelayRequestEnvelope =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(matches!(
            request.request,
            RelayRequest::ProjectMemoryTransfer {
                action: Action::BeginTurnSnapshot { .. }
            }
        ));
        let response = RelayResponseEnvelope {
            request_id: request.request_id,
            protocol_version: RELAY_PROTOCOL_VERSION,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::ProjectMemoryTransfer {
                    result: Reply::Busy,
                },
            },
        };
        let mut frame = serde_json::to_vec(&response).unwrap();
        frame.push(b'\n');
        writer.write_all(&frame).await.unwrap();
    });
    let error = client
        .fence_project_memory_snapshot(
            Some("absorbed".into()),
            mj_core::project_memory::ProjectMemorySnapshot::default().version(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("busy"));
    peer.await.unwrap();
}

// Hard-won: 0d930fcd3: a Busy Snapshot did not retire an earlier turn's handout.
#[tokio::test]
async fn busy_turn_snapshot_is_not_a_completed_sync() {
    use mj_core::relay::ProjectMemoryTransferResult as Reply;
    let temp = tempfile::tempdir().unwrap();
    let target = crate::session_manager::ProjectMemorySyncTarget {
        canonical_root: temp.path().join("canonical/memory"),
        relay: CommandSpec::new("unused", ["unused"]),
    };
    let (controller, worker) = tokio::io::duplex(8192);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut lines = BufReader::new(reader).lines();
        let request: RelayRequestEnvelope =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let response = RelayResponseEnvelope {
            request_id: request.request_id,
            protocol_version: RELAY_PROTOCOL_VERSION,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::ProjectMemoryTransfer {
                    result: Reply::Busy,
                },
            },
        };
        let mut frame = serde_json::to_vec(&response).unwrap();
        frame.push(b'\n');
        writer.write_all(&frame).await.unwrap();
    });
    let completed = crate::session_manager::sync_turn_project_memory(
        &mut client,
        &target,
        SESSION_ID,
        &mj_core::config::Config::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!completed);
    peer.await.unwrap();
}

// Hard-won: #1542: a 15 s per-RPC deadline that counted worker queueing failed Suspend on a loaded host
#[tokio::test(start_paused = true)]
async fn project_memory_queueing_has_no_deadline_and_remains_cancellable() {
    use mj_core::project_memory::ProjectMemorySnapshot;
    use mj_core::relay::{
        ProjectMemoryTransferAction as Action, ProjectMemoryTransferResult as Reply,
    };

    for outcome in ["complete", "drop", "disconnect", "legacy_drop"] {
        let legacy = outcome == "legacy_drop";
        let (controller, worker) = tokio::io::duplex(4096);
        let mut client = duplex_relay_client(controller, SESSION_ID);
        if legacy {
            client.protocol_version = mj_core::relay::RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL - 1;
        }
        let queue = Arc::new(tokio::sync::Semaphore::new(0));
        let worker_queue = queue.clone();
        let (queued, mut waiting) = mpsc::channel(1);
        let peer = mj_core::runtime::spawn(async move {
            let bytes = mj_core::relay::ProjectMemoryWireSnapshot::encode(
                ProjectMemorySnapshot::default(),
                ProjectMemorySnapshot::default(),
                RELAY_PROTOCOL_VERSION,
            )
            .unwrap();
            let (reader, mut writer) = tokio::io::split(worker);
            let mut reader = BufReader::new(reader);
            for page in [false, true] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
                let expected = if page {
                    Action::ReadSnapshotPage {
                        transfer_id: 1,
                        offset: 0,
                    }
                } else {
                    Action::BeginSnapshot
                };
                if legacy {
                    assert_eq!(request.request, RelayRequest::ProjectMemorySnapshot);
                } else {
                    assert_eq!(
                        request.request,
                        RelayRequest::ProjectMemoryTransfer { action: expected }
                    );
                }
                // Model the worker's I/O semaphore queue before it can reply.
                queued.send(()).await.unwrap();
                worker_queue.acquire().await.unwrap().forget();
                let result = if page {
                    Reply::SnapshotPage {
                        transfer_id: 1,
                        offset: 0,
                        next_offset: bytes.len() as u64,
                        complete: true,
                        chunk: BASE64.encode(&bytes),
                    }
                } else {
                    Reply::SnapshotStarted {
                        transfer_id: 1,
                        total_bytes: bytes.len() as u64,
                    }
                };
                let response = RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version: RELAY_PROTOCOL_VERSION,
                    body: RelayResponseBody::Ok {
                        payload: RelayResponsePayload::ProjectMemoryTransfer { result },
                    },
                };
                let mut frame = serde_json::to_vec(&response).unwrap();
                frame.push(b'\n');
                writer.write_all(&frame).await.unwrap();
            }
            let mut extra = String::new();
            reader.read_line(&mut extra).await.unwrap();
            assert!(extra.is_empty(), "unexpected abort after transfer: {extra}");
        });
        let dropped = tokio_util::sync::CancellationToken::new();
        let drop_transfer = dropped.clone();
        let transfer = mj_core::runtime::spawn(async move {
            let result = tokio::select! {
                result = client.project_memory_snapshot() => result,
                () = drop_transfer.cancelled() => Err(anyhow!("checkpoint was cancelled")),
            };
            (client, result)
        });
        waiting.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(16)).await;
        assert!(
            !transfer.is_finished(),
            "snapshot start expired in the worker queue"
        );
        if !legacy {
            queue.add_permits(1);
            waiting.recv().await.unwrap();
            tokio::time::advance(Duration::from_secs(16)).await;
            assert!(
                !transfer.is_finished(),
                "snapshot page expired in the worker queue"
            );
        }
        match outcome {
            "complete" => queue.add_permits(1),
            "drop" | "legacy_drop" => dropped.cancel(),
            "disconnect" => {
                peer.abort();
            }
            _ => unreachable!(),
        }
        let (client, result) = if outcome == "complete" {
            transfer.await.unwrap()
        } else {
            tokio::time::timeout(Duration::from_millis(100), transfer)
                .await
                .expect("cancellation or disconnect must not wait for the worker queue")
                .unwrap()
        };
        if outcome == "complete" {
            assert_eq!(
                result.unwrap(),
                (
                    ProjectMemorySnapshot::default(),
                    ProjectMemorySnapshot::default()
                )
            );
            assert!(client.abandoned.is_none());
        } else {
            let error = result.unwrap_err();
            if outcome == "disconnect" {
                assert!(error.downcast_ref::<RelayTransportDead>().is_some());
            } else {
                assert_eq!(error.to_string(), "checkpoint was cancelled");
            }
            assert!(!format!("{error:#}").contains("abort project-memory transfer failed"));
            assert!(client.abandoned.is_some());
            assert!(client.input.is_none());
        }
        drop(client);
        if outcome == "complete" {
            peer.await.unwrap();
        } else if outcome != "disconnect" {
            peer.abort();
            assert!(peer.await.unwrap_err().is_cancelled());
        } else {
            assert!(peer.await.unwrap_err().is_cancelled());
        }
    }
}

// Hard-won: 0187f165f: a stalled initial snapshot prevented checkpoint preemption.
#[tokio::test]
async fn checkpoint_preempts_turn_snapshot_start_and_closes_transfer() {
    use mj_core::project_memory::ProjectMemorySnapshot;
    use mj_core::relay::ProjectMemoryTransferAction as Action;

    let (controller, worker) = tokio::io::duplex(4096);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let slot = Arc::new(mj_core::recovery_gate::ProjectMemorySyncSlot::default());
    let (guard, cancel) = slot.start_turn().unwrap();
    let (queued, waiting) = tokio::sync::oneshot::channel();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, _writer) = tokio::io::split(worker);
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            request.request,
            RelayRequest::ProjectMemoryTransfer {
                action: Action::BeginTurnSnapshot { .. }
            }
        ));
        queued.send(()).unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            line.is_empty(),
            "cancelled transfer must close its connection"
        );
    });
    let turn = mj_core::runtime::spawn(async move {
        let _guard = guard;
        let result = client
            .project_memory_turn_snapshot(None, ProjectMemorySnapshot::default().version(), &cancel)
            .await;
        (client, result)
    });
    waiting.await.unwrap();
    let checkpoint = tokio::time::timeout(Duration::from_secs(1), slot.checkpoint())
        .await
        .expect("checkpoint must preempt the stalled snapshot");
    let (client, result) = turn.await.unwrap();
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert!(client.abandoned.is_some());
    assert!(client.input.is_none());
    drop(client);
    peer.await.unwrap();
    drop(checkpoint);
}

/// A job the actor refuses while its connection is leased makes the actor wake
/// the coordinator when the lease returns, without retaining the job.
#[cfg(unix)]
#[tokio::test]
async fn a_login_deferred_by_an_actor_lease_is_pushed_when_the_lease_returns() {
    const CHILD: &str = "MJ_TEST_CREDENTIAL_ACTOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            "a_login_deferred_by_an_actor_lease_is_pushed_when_the_lease_returns",
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "credential-lease-wake")
        .isolated_store(directory.path())
        .run();
        return;
    }
    // The coordinator's own disposition line is how this test sees its job
    // refused, so the log is captured for every thread.
    let log = CapturedLog::default();
    tracing::subscriber::set_global_default(log.clone()).expect("the only global subscriber");
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::save_session(&crate::database::test_fixtures::checkpoint_test_session(
        SESSION_ID,
    ))
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let received = scratch.path().join("received");
    let mut target = codex_sync_target(
        home.path(),
        SESSION_ID,
        "2026-09-25T20:00:00.000Z",
        &received,
    );
    // This fixture needs no newer feeds; its actor still owns the real relay connection.
    target.spec.args[2] = "16".into();
    target.spec.env.insert(
        "MJ_TEST_CREDENTIAL_ACTOR_STATE".into(),
        serde_json::to_string(
            &mj_core::relay::RelaySnapshot::new(SESSION_ID.into()).operational_state(),
        )
        .unwrap(),
    );
    let login = codex_login("2026-09-25T22:49:05.000Z");
    std::fs::write(home.path().join("auth.json"), &login).unwrap();
    credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![crate::session_manager::RelaySessionTarget {
            session_id: SESSION_ID.into(),
            spec: target.spec.clone(),
            worker_recovery: None,
            project_memory: None,
        }])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(SESSION_ID, Duration::from_secs(10))
        .await
        .unwrap();
    let gate = Arc::new(crate::recovery_gate::RecoveryGate::default());
    let copy = gate
        .try_start(
            SESSION_ID,
            crate::recovery_gate::BackgroundWork::DelegationDelivery,
        )
        .unwrap();
    let lease = handle.lease_connection(&copy).await.unwrap();
    let registry = Arc::new(std::sync::Mutex::new(
        mj_core::credentials::RejectedLogins::default(),
    ));
    let sync = CredentialSyncCoordinator::spawn_with_registry(
        SessionRelays::Actors(channels.control.clone()),
        None,
        credential_sync::unknown_slopcop_login,
        mj_core::credentials::SYNC_INTERVAL,
        registry.clone(),
    );
    let sync_handle = sync.handle();
    sync_handle.set_targets(vec![target]);
    // The coordinator's own job is refused while capture holds the actor's
    // connection; the actor then owes the session a sync.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !log.at_or_above(tracing::Level::DEBUG).iter().any(|line| {
            line.contains("credential sync job refused by the session actor")
                && line.contains("holds the relay connection")
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the credential sync job must be refused while capture holds the connection");
    assert!(!received.exists());
    assert!(!copy.load(Ordering::Acquire));
    let released = tokio::time::Instant::now();
    lease.release();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !received.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lease return must wake the profile pass immediately");
    assert!(released.elapsed() < mj_core::credentials::SYNC_INTERVAL);
    assert_eq!(std::fs::read(&received).unwrap(), login);
    let mut sync = sync;
    let result = tokio::time::timeout(Duration::from_secs(5), sync.result())
        .await
        .unwrap()
        .unwrap();
    assert!(result.pushed_to(SESSION_ID));
    assert!(matches!(
        registry
            .lock()
            .unwrap()
            .delivery_state("codex4", SESSION_ID),
        Some(mj_core::credentials::LoginDeliveryState::Concluded { .. }),
    ));
    assert!(
        !copy.load(Ordering::Acquire),
        "credential wake must not preempt the copy"
    );
    sync_handle.set_targets(Vec::new());
    drop(sync_handle);
    drop(sync);
    drop(copy);
    channels.shutdown.shutdown().await.unwrap();
}

/// A renewal revokes the old token within seconds, so the new login must reach
/// a session whatever other background work holds it: a review of its work, a
/// worker upgrade, or a recovery copy that can run for minutes.
// Hard-won: #1600: a review or worker-upgrade hold kept a renewed login from reaching the session
#[cfg(unix)]
#[tokio::test]
async fn a_renewed_login_reaches_a_session_whatever_background_work_holds_it() {
    use crate::recovery_gate::BackgroundWork;
    const CHILD: &str = "MJ_TEST_RENEWED_LOGIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            "a_renewed_login_reaches_a_session_whatever_background_work_holds_it",
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "renewed-login-holders")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    crate::database::save_session(&crate::database::test_fixtures::checkpoint_test_session(
        SESSION_ID,
    ))
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let received = scratch.path().join("received");
    let mut target = codex_sync_target(
        home.path(),
        SESSION_ID,
        "2026-09-25T20:00:00.000Z",
        &received,
    );
    // This fixture needs no newer feeds; its actor still owns the real relay connection.
    target.spec.args[2] = "16".into();
    target.spec.env.insert(
        "MJ_TEST_CREDENTIAL_ACTOR_STATE".into(),
        serde_json::to_string(
            &mj_core::relay::RelaySnapshot::new(SESSION_ID.into()).operational_state(),
        )
        .unwrap(),
    );
    let auth = home.path().join("auth.json");
    std::fs::write(&auth, codex_login("2026-09-25T21:00:00.000Z")).unwrap();
    credential_sync::SESSIONS_ARE_CURRENT.with(|current| current.set(true));
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![crate::session_manager::RelaySessionTarget {
            session_id: SESSION_ID.into(),
            spec: target.spec.clone(),
            worker_recovery: None,
            project_memory: None,
        }])
        .unwrap();
    channels
        .control
        .wait_for_session(SESSION_ID, Duration::from_secs(10))
        .await
        .unwrap();
    // The gate the session's own actor uses, so a holder is a real one.
    let gate = channels.control.session_gate.clone();
    let sync = CredentialSyncCoordinator::spawn_with_registry(
        SessionRelays::Actors(channels.control.clone()),
        None,
        credential_sync::unknown_slopcop_login,
        mj_core::credentials::SYNC_INTERVAL,
        Default::default(),
    );
    let sync_handle = sync.handle();
    sync_handle.set_targets(vec![target.clone()]);
    let delivered = |login: Vec<u8>| {
        let received = received.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while std::fs::read(&received).ok().as_deref() != Some(login.as_slice()) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok()
        }
    };
    assert!(delivered(codex_login("2026-09-25T21:00:00.000Z")).await);

    // A review and a worker upgrade refuse each other, so they are separate cases.
    for (case, minute) in ["review", "worker upgrade", "delegation delivery"]
        .into_iter()
        .zip(10_u32..)
    {
        // The hold, kept until the end of the case, and the flag a lifecycle
        // operation would set to preempt it.
        let (holder, preempted): (Box<dyn std::any::Any>, _) = match case {
            "review" => (
                Box::new(gate.reserve_when_idle(SESSION_ID).await),
                Arc::new(AtomicBool::new(false)),
            ),
            "worker upgrade" => {
                let attempt = gate
                    .try_start(SESSION_ID, BackgroundWork::WorkerReplacement)
                    .unwrap();
                let flag = attempt.cancellation();
                (Box::new(attempt) as _, flag)
            }
            // Hard-won: #1564: back-to-back recovery copies held the session's only background slot, so a renewed login never reached it.
            _ => {
                let attempt = gate
                    .try_start(SESSION_ID, BackgroundWork::DelegationDelivery)
                    .unwrap();
                let flag = attempt.cancellation();
                (Box::new(attempt) as _, flag)
            }
        };
        // Another daemon renews: it writes the new login under the native
        // lock, and this daemon is told.
        let renewed = codex_login(&format!("2026-09-25T22:{minute}:00.000Z"));
        let renewer = mj_core::credentials::CredentialWriteGuard::acquire(
            mj_core::config::HarnessKind::Codex,
            &auth,
        )
        .await
        .unwrap();
        std::fs::write(&auth, &renewed).unwrap();
        drop(renewer);
        sync_handle.sync_profile_now("codex4", None);
        assert!(
            delivered(renewed).await,
            "the renewed login did not reach the session while a {case} held it"
        );
        assert!(
            !preempted.load(Ordering::Acquire),
            "the sync cancelled the {case}"
        );
        drop(holder);
    }
    sync_handle.set_targets(Vec::new());
    drop(sync_handle);
    drop(sync);
    channels.shutdown.shutdown().await.unwrap();
}

/// Reconcile one Claude session against the profile's shared login, the way
/// the triggering session of an authentication-failure pass is: `pinned` is
/// the reading that pass took.
#[cfg(all(unix, not(target_os = "macos")))]
async fn judged_token(
    profile: &Arc<credential_sync::ProfileLogin>,
    target: &CredentialSyncTarget,
    pinned: Option<Arc<credential_sync::CanonicalLogin>>,
) -> Option<String> {
    let skills = Arc::new(CanonicalSkills::collect(target));
    let pass = credential_sync::PassLogin::Shared(profile.clone());
    reconcile_session(
        &SessionRelays::Direct,
        target,
        &skills,
        &credential_sync::GithubSync::Known(None),
        credential_sync::HostLogins {
            slopcop: &credential_sync::HostSlopcopLogin::Unknown,
            harness: &pass,
            pinned,
        },
        credential_sync::JobHold::new(None, None),
    )
    .await
    .unwrap()
    .unwrap()
    .current_access_token
}

/// A Claude profile whose host login is on disk and whose session holds an
/// older copy.
#[cfg(all(unix, not(target_os = "macos")))]
fn claude_profile_and_target(
    scratch: &std::path::Path,
) -> (
    Arc<credential_sync::ProfileLogin>,
    CredentialSyncTarget,
    String,
) {
    let host = claude_login(5_000_000_000_000, "access-host", Some("refresh-host"));
    std::fs::write(scratch.join(".credentials.json"), &host).unwrap();
    let session = claude_login(1_000, "access-old", None);
    let target = claude_sync_target(scratch, SESSION_ID, &session, &scratch.join("received"));
    let fingerprint = mj_core::credentials::claude_access_token_fingerprint(&host).unwrap();
    (
        Arc::new(credential_sync::ProfileLogin::new(None, Default::default())),
        target,
        fingerprint,
    )
}

// Hard-won: #1600: lock evidence from an earlier failure was reused for a later one
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "multi_thread")]
async fn an_earlier_failures_lock_evidence_never_judges_a_later_failure() {
    let scratch = tempfile::tempdir().unwrap();
    let (profile, target, _) = claude_profile_and_target(scratch.path());
    // An earlier failure's pass read the login under the lock.
    let (_, earlier) = profile.refresh(None, &target, true).await;
    assert!(earlier.is_some());
    // A later failure's locked read timed out, so its pass fell back to a
    // plain reading of the same login: it is unknown, whatever the earlier
    // pass found.
    let (_, later) = profile.refresh(None, &target, false).await;
    assert_eq!(judged_token(&profile, &target, later).await, None);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "multi_thread")]
async fn a_failures_verdict_uses_its_own_locked_reading_not_a_later_unlocked_one() {
    let scratch = tempfile::tempdir().unwrap();
    let (profile, target, fingerprint) = claude_profile_and_target(scratch.path());
    let (_, own) = profile.refresh(None, &target, true).await;
    // Another pass publishes an unlocked reading of the same login first.
    profile.refresh(None, &target, false).await;
    assert_eq!(
        judged_token(&profile, &target, own).await,
        Some(fingerprint),
        "another pass's reading erased the pass's own lock evidence"
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "multi_thread")]
async fn the_newest_login_is_delivered_and_judged_when_another_pass_replaced_the_pinned_one() {
    let scratch = tempfile::tempdir().unwrap();
    let (profile, target, fingerprint_a) = claude_profile_and_target(scratch.path());
    let (_, own) = profile.refresh(None, &target, true).await;
    // The login is replaced (B) and another pass publishes it. Delivery is
    // the newest login, so the verdict compares the rejected A with B.
    let login_b = claude_login(6_000_000_000_000, "access-b", Some("refresh-b"));
    std::fs::write(scratch.path().join(".credentials.json"), &login_b).unwrap();
    let fingerprint_b = mj_core::credentials::claude_access_token_fingerprint(&login_b).unwrap();
    assert_ne!(fingerprint_a, fingerprint_b);
    // B read without the lock is not evidence, and the pinned A does not
    // describe it: unknown.
    profile.refresh(None, &target, false).await;
    assert_eq!(judged_token(&profile, &target, own.clone()).await, None);
    // B read under the lock is: the verdict sees B, not the rejected A.
    profile.refresh(None, &target, true).await;
    assert_eq!(
        judged_token(&profile, &target, own).await,
        Some(fingerprint_b)
    );
}

// Hard-won: #1581: a slow credential RPC delayed delivery to all later sessions.
#[cfg(unix)]
#[tokio::test]
async fn credential_fanout_pushes_to_a_peer_before_the_blocked_worker_replies() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let waiting = scratch.path().join("waiting");
    let release = scratch.path().join("release");
    let slow_copy = scratch.path().join("slow-copy");
    let fast_copy = scratch.path().join("fast-copy");
    let old = "2026-09-25T20:00:00Z";
    let login = codex_login("2026-09-25T21:00:00Z");
    std::fs::write(home.path().join("auth.json"), &login).unwrap();
    let mut slow = codex_sync_target(home.path(), "a-slow", old, &slow_copy);
    slow.spec.env.extend([
        (
            "MJ_TEST_CREDENTIAL_WAITING".into(),
            waiting.to_string_lossy().into_owned(),
        ),
        (
            "MJ_TEST_CREDENTIAL_RELEASE".into(),
            release.to_string_lossy().into_owned(),
        ),
    ]);
    let fast = codex_sync_target(home.path(), "b-fast", old, &fast_copy);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![slow, fast]);

    tokio::time::timeout(Duration::from_secs(5), async {
        while !waiting.exists() || !fast_copy.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the peer received the current login while the first relay was blocked");
    assert!(!slow_copy.exists());
    std::fs::write(release, "").unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
        .await
        .unwrap()
        .unwrap();
    assert!(result.pushed_to("a-slow"));
    assert!(result.pushed_to("b-fast"));
    assert_eq!(std::fs::read(fast_copy).unwrap(), login);
    assert_eq!(std::fs::read(slow_copy).unwrap(), login);
}

// Hard-won: #1581: a credential read-back mismatch could cause repeated installs.
#[cfg(unix)]
#[tokio::test]
async fn credential_install_readback_mismatch_concludes_the_delivery() {
    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let received = scratch.path().join("held");
    let attempts = scratch.path().join("attempts");
    let mismatch = scratch.path().join("mismatch");
    let old = "2026-09-25T20:00:00Z";
    let unexpected = codex_login("2026-09-25T20:30:00Z");
    std::fs::write(&mismatch, &unexpected).unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        codex_login("2026-09-25T21:00:00Z"),
    )
    .unwrap();
    let mut target = codex_sync_target(home.path(), "readback", old, &received);
    target.spec.args.extend([
        String::new(),
        String::new(),
        String::new(),
        mismatch.to_string_lossy().into_owned(),
    ]);
    target.spec.env.insert(
        "MJ_TEST_CREDENTIAL_INSTALLS".into(),
        attempts.to_string_lossy().into_owned(),
    );
    let registry = Arc::new(std::sync::Mutex::new(
        mj_core::credentials::RejectedLogins::default(),
    ));
    let mut coordinator = CredentialSyncCoordinator::spawn_with_registry(
        SessionRelays::Direct,
        None,
        credential_sync::unknown_slopcop_login,
        SYNC_INTERVAL,
        registry.clone(),
    );
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let failed = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
        .await
        .unwrap()
        .unwrap();
    assert!(
        failed
            .failures()
            .any(|(_, _, detail)| detail.contains("did not retain"))
    );
    assert!(matches!(
        registry
            .lock()
            .unwrap()
            .delivery_state(&target.profile_id, &target.session_id),
        Some(mj_core::credentials::LoginDeliveryState::Concluded { .. }),
    ));
    handle.sync_profile_now(&target.profile_id, None);
    tokio::time::timeout(Duration::from_secs(5), coordinator.result())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&received).unwrap(), unexpected);
    assert_eq!(
        std::fs::read_to_string(attempts).unwrap().lines().count(),
        1
    );
}

/// AWS's recorded refusal, as a file the stand-in `aws` can print.
#[cfg(unix)]
fn access_denied_reply(directory: &std::path::Path) -> std::path::PathBuf {
    let path = directory.join("access-denied.txt");
    std::fs::write(&path, crate::aws_assume_role::test_fixtures::ACCESS_DENIED).unwrap();
    path
}

#[tokio::test(start_paused = true)]
async fn adoption_wait_reports_blockers_without_a_deadline_and_is_cancellable() {
    for cancel in [false, true] {
        let (controller, worker) = tokio::io::duplex(4096);
        let mut client = duplex_relay_client(controller, SESSION_ID);
        let cancelled = tokio_util::sync::CancellationToken::new();
        let cancellation = cancelled.clone();
        let (progress, mut messages) = tokio::sync::watch::channel(None);
        client.set_adoption_wait_progress(Some(progress));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let released = release.clone();
        let peer = mj_core::runtime::spawn(async move {
            let (reader, mut writer) = tokio::io::split(worker);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let envelope: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            for payload in [
                RelayResponsePayload::LiveBaselineWaiting {
                    blockers: vec!["a running turn".into()],
                },
                RelayResponsePayload::LiveBaselineStarted { total_bytes: 0 },
            ] {
                if matches!(payload, RelayResponsePayload::LiveBaselineStarted { .. }) {
                    let _permit = released.acquire().await.unwrap();
                }
                let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
                    request_id: envelope.request_id.clone(),
                    protocol_version: envelope.protocol_version,
                    body: RelayResponseBody::Ok { payload },
                })
                .unwrap();
                bytes.push(b'\n');
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let work = mj_core::runtime::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => anyhow::bail!("adoption cancelled"),
                result = client.call(RelayRequest::BeginLiveBaseline) => result,
            }
        });
        messages.changed().await.unwrap();
        assert_eq!(
            messages.borrow_and_update().as_deref(),
            Some(["a running turn".to_owned()].as_slice())
        );
        tokio::time::advance(RELAY_RPC_TIMEOUT * 4).await;
        assert!(!work.is_finished());
        if cancel {
            cancelled.cancel();
            assert!(work.await.unwrap().is_err());
        } else {
            release.add_permits(1);
            assert!(matches!(
                work.await.unwrap().unwrap(),
                RelayResponsePayload::LiveBaselineStarted { .. }
            ));
        }
        peer.abort();
        let _ = peer.await;
    }
}

// Hard-won: #1673: a remote harness reload answered after the 15 s relay deadline was abandoned although the worker went on to commit it.
#[tokio::test(start_paused = true)]
async fn in_place_reload_waits_for_the_worker_past_the_control_deadline() {
    let (controller, worker) = tokio::io::duplex(65536);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let released = release.clone();
    let requested = Arc::new(tokio::sync::Notify::new());
    let request_seen = requested.clone();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let envelope: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            envelope.request,
            RelayRequest::RestartHarnessInPlace { .. }
        ));
        request_seen.notify_one();
        let _permit = released.acquire().await.unwrap();
        let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
            request_id: envelope.request_id,
            protocol_version: envelope.protocol_version,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::HarnessRestartedInPlace,
            },
        })
        .unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
    });
    let launch: mj_core::worker_launch::WorkerLaunchConfig =
        serde_json::from_value(serde_json::json!({
            "session_id": SESSION_ID,
            "harness": "claude",
            "bridge_command": "/usr/bin/false",
            "bridge_args": [],
            "environment": {},
            "cwd": "workspace",
            "execution_policy": true,
        }))
        .unwrap();
    let work = mj_core::runtime::spawn(async move {
        let result = client
            .restart_harness_in_place(
                "move-1",
                "store-1",
                Box::new(launch),
                Some("candidate-1".into()),
            )
            .await;
        (client, result)
    });
    requested.notified().await;
    tokio::time::advance(RELAY_RPC_TIMEOUT * 4).await;
    assert!(
        !work.is_finished(),
        "the control deadline must not cover the reload"
    );
    release.add_permits(1);
    let (client, result) = work.await.unwrap();
    result.unwrap();
    assert!(client.abandoned.is_none());
    peer.await.unwrap();
}

// Hard-won: #1625: cancelling RelayClient::connect waited for detach grace on a detached thread and leaked sleep proxies on process exit.
#[cfg(unix)]
#[tokio::test]
async fn cancelling_connect_before_handshake_stops_proxy_immediately() {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().unwrap();
    let fifo = temp.path().join("proxy-alive.fifo");
    let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_fifo` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);

    let mut spec = CommandSpec::new("sh", ["-c", "exec 3>\"$FIFO\"; exec sleep 600"]);
    spec.env
        .insert("FIFO".into(), fifo.to_string_lossy().into_owned());

    let fifo_for_reader = fifo.clone();
    let opened = mj_core::runtime::spawn_blocking(move || std::fs::File::open(fifo_for_reader));

    let connect = mj_core::runtime::spawn(async move {
        RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(30)).await
    });
    let mut reader = tokio::time::timeout(Duration::from_secs(5), opened)
        .await
        .expect("proxy must open the sentinel FIFO")
        .unwrap()
        .unwrap();

    connect.abort();
    let _ = connect.await;

    // The kernel closes the proxy's write end of the FIFO as soon as the proxy
    // is killed, even if container PID 1 does not reap zombies. Confirm it
    // closes well before RELAY_PROXY_DETACH_GRACE.
    let eof = tokio::time::timeout(
        Duration::from_millis(500),
        mj_core::runtime::spawn_blocking(move || {
            let mut byte = [0_u8; 1];
            reader.read(&mut byte)
        }),
    )
    .await
    .expect("cancelled connect must kill its proxy immediately without waiting for detach grace")
    .unwrap()
    .unwrap();
    assert_eq!(eof, 0, "killed proxy must close the sentinel FIFO");
}

#[cfg(unix)]
#[tokio::test]
async fn aws_renewal_uses_the_pinned_profile_for_scoped_and_unscoped_sessions() {
    use mj_core::config::{Config, HarnessKind, HarnessProfile};
    use mj_core::state::{State, TargetLocator};
    use std::os::unix::fs::PermissionsExt;

    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    let token = scratch.path().join("token");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let export = scratch.path().join("export.json");
    std::fs::write(
        &export,
        br#"{"Version":1,"AccessKeyId":"exported","SecretAccessKey":"secret"}"#,
    )
    .unwrap();
    let calls = scratch.path().join("calls");
    std::fs::write(
        cli.join("aws"),
        format!(
            "#!/bin/sh\n/bin/echo \"$1 $AWS_PROFILE\" >> {calls}\ncase \"$1\" in\nconfigure) exec /bin/cat {export};;\nsts) exec /bin/cat {assumed};;\nesac\n",
            calls = calls.display(),
            export = export.display(),
            assumed = scratch.path().join("granted.json").display(),
        ),
    ).unwrap();
    std::fs::set_permissions(cli.join("aws"), std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut config = Config::default().with_local_targets();
    let mut profile = HarnessProfile::new(HarnessKind::Codex, &home);
    profile
        .environment
        .insert("PATH".into(), cli.to_string_lossy().into_owned());
    let mut state = State::default();
    let source_config = home.join("config.toml");
    let bedrock = "model_provider = \"amazon-bedrock-runtime\"\n[model_providers.amazon-bedrock-runtime.aws]\n";
    for (scoped, changed) in [(false, "from-file"), (true, "from-environment")] {
        profile.aws = scoped.then(role_config);
        profile
            .environment
            .insert("AWS_PROFILE".into(), "from-environment".into());
        std::fs::write(
            &source_config,
            if scoped {
                bedrock.to_owned()
            } else {
                format!("{bedrock}profile = \"from-file\"\n")
            },
        )
        .unwrap();
        config.profiles.insert("codex".into(), profile.clone());

        for (generation, expected) in [("existing", "original"), ("fresh", changed)] {
            let id = format!("{generation}-{changed}");
            let mut session = crate::database::test_fixtures::checkpoint_test_session(&id);
            session.target_template_id = "docker".into();
            session.target = Some(Box::new(TargetLocator::LocalDocker {
                container_id: "c".repeat(64),
                borrowed_from: None,
            }));
            let root = scratch.path().join(&id);
            std::fs::create_dir(&root).unwrap();
            state.sessions.insert(id.clone(), session);
            let staged = if generation == "existing" {
                "original".to_owned()
            } else {
                mj_worker_launch::worker_launch::aws_credential_source(&config, &state, &id)
                    .unwrap()
                    .unwrap()
                    .environment["AWS_PROFILE"]
                    .clone()
            };
            state.sessions.get_mut(&id).unwrap().staged_aws_profile = Some(staged);
            let mut target = aws_sync_session_named(&home, &root, &token, &cli, &id, None);
            target.aws_credentials =
                mj_worker_launch::worker_launch::aws_credential_sync_source(&config, &state, &id)
                    .unwrap();
            let outcomes = reconcile_profile(&[target], None).await;
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| outcome.step_failures.is_empty()),
                "{outcomes:?}"
            );
            let installed: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("aws")).unwrap()).unwrap();
            assert_eq!(installed["Profile"], expected);
            assert_eq!(root.join("aws-scope.json").exists(), scoped);
        }
    }

    let calls = std::fs::read_to_string(calls).unwrap();
    assert_eq!(
        calls,
        "configure original\nconfigure from-file\nsts original\nsts from-environment\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_attachment_submission_refuses_protocol_89_before_writing_a_request() {
    let script = format!(
        r#"python3 -c '
import json, sys
req = json.loads(sys.stdin.readline())
print(json.dumps({{"request_id":req["request_id"], "protocol_version":89, "result":"ok", "payload":{{"type":"hello", "data":{{"negotiated":89, "relay_version":"old", "session_id":{SESSION_ID:?}}}}}}}), flush=True)
sys.stdin.read()
'"#
    );
    let spec = CommandSpec::new("sh", ["-c", &script]).purpose("protocol 89 attachment test");
    let mut client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .unwrap();
    let reference =
        mj_core::attachment::FileAttachmentRef::new(b"abc", "input.txt", "text/plain".into())
            .unwrap();
    let before = client.next_request;
    let error = client
        .submit(
            "file-prompt",
            RelayCommand::Prompt {
                prompt: vec![reference.content_block()],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<mj_core::refusal::Refusal>(),
        Some(&mj_core::refusal::Refusal::precondition(
            "this session's worker predates file attachments; restart the session to attach files",
        ))
    );
    assert_eq!(client.next_request, before);
}

#[cfg(unix)]
// Hard-won: #1675: Create readiness persisted relay events before Create committed
#[tokio::test]
async fn create_readiness_uses_one_status_only_relay_at_the_negotiated_protocol() {
    use crate::readiness::{NativeSessionProbe, NativeSessionReadiness};
    use mj_core::relay::{
        HarnessPreparation, RELAY_WORKER_STARTUP_DEADLINE_PROTOCOL, RelayExecutionState,
    };

    let directory = tempfile::tempdir().unwrap();
    let relay = DurableRelay::open(directory.path(), SESSION_ID, "1.0.0").unwrap();
    let mut ready = relay.operational_state();
    ready.native_session_id = Some("native".into());
    ready.acp_ready = Some(true);
    let mut preparing = ready.clone();
    preparing.harness_preparation = Some(HarnessPreparation::Preparing {
        step: "install".into(),
        since_ms: 3,
    });
    let mut closed = preparing.clone();
    closed.execution = RelayExecutionState::Closed;
    let mut failed = closed.clone();
    failed.harness_preparation = Some(HarnessPreparation::Failed {
        step: "install".into(),
        error: "unavailable".into(),
        at_ms: 3,
    });
    let statuses = directory.path().join("statuses.json");
    std::fs::write(
        &statuses,
        serde_json::to_vec(&[preparing, ready, closed, failed]).unwrap(),
    )
    .unwrap();
    let protocol = RELAY_WORKER_STARTUP_DEADLINE_PROTOCOL - 1;
    let fixture = r#"
import json, sys
protocol = int(sys.argv[1])

def respond(method, payload):
    request = json.loads(sys.stdin.readline())
    assert request["request"]["method"] == method, request
    print(json.dumps(dict(request_id=request["request_id"], protocol_version=protocol,
                          result="ok", payload=dict(type=method, data=payload))), flush=True)

respond("hello", dict(negotiated=protocol, relay_version="old", session_id=sys.argv[2]))
with open(sys.argv[3]) as statuses:
    for status in json.load(statuses):
        respond("status", status)
assert not sys.stdin.readline()
"#;
    let spec = CommandSpec::new(
        "python3",
        [
            "-c".into(),
            fixture.into(),
            protocol.to_string(),
            SESSION_ID.into(),
            statuses.to_string_lossy().into_owned(),
        ],
    );
    let mut client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(NativeSessionProbe::protocol_version(&client), protocol);
    let next_request = client.next_request;
    assert!(matches!(
        client.native_session_readiness().await.unwrap(),
        NativeSessionReadiness::Preparing { step, since_ms: 3 } if step == "install"
    ));
    assert!(matches!(
        client.native_session_readiness().await.unwrap(),
        NativeSessionReadiness::Ready(id) if id == "native"
    ));
    assert!(matches!(
        client.native_session_readiness().await.unwrap(),
        NativeSessionReadiness::Closed
    ));
    assert!(matches!(
        client.native_session_readiness().await.unwrap(),
        NativeSessionReadiness::PreparationFailed { step, error }
            if step == "install" && error == "unavailable"
    ));
    assert_eq!(client.next_request, next_request + 4);
}

// Exercises connection ownership at the relay protocol boundary: a snapshot
// exists only in the proxy that began it, and failed bulk work must not poison
// the control stream.
#[cfg(unix)]
#[test]
fn bulk_transfers_preserve_control_connection_ownership() {
    const CHILD: &str = "MJ_BULK_LANE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            "bulk_transfers_preserve_control_connection_ownership",
        ))
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    mj_core::targets::set_ssh_connection_sharing_for_test(Some(
        mj_core::targets::SshSharingForTest::Disabled,
    ));
    let captured = CapturedLog::default();
    tracing::subscriber::set_global_default(captured.clone()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests");
    let script = r#"
import sys, json, os, base64
session, log, protocol = sys.argv[1], sys.argv[2], int(sys.argv[3])
snapshot = None
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    params = req["request"].get("params", {})
    with open(log, "a") as f: f.write(str(os.getpid()) + " " + method + "\n")
    if method == "hello":
        payload = {"type":"hello", "data":{"negotiated":protocol,"relay_version":"bulk-test","session_id":session}}
    elif method in ("skills_state", "install_skills"):
        if method == "install_skills" and base64.b64decode(params["data"]) == b"fail":
            sys.exit(1)
        payload = {"type":"skills_state","data":{"present":True,"fingerprint":"test"}}
    elif method in ("attachment_present", "file_attachment_present"):
        payload = {"type":"attachment_present","data":{"present":False}}
    elif method in ("install_attachment", "install_file_attachment"):
        payload = {"type":"attachment_installed"}
    elif method == "submit":
        payload = {"type":"accepted","data":{"command_id":params["command_id"],"ordinal":1}}
    elif method in ("read_attachment", "read_file_attachment"):
        filename = params["reference"].get("filename", "")
        if filename == "disconnect":
            sys.exit(1)
        if filename == "reject":
            print(json.dumps({"request_id":req["request_id"],"protocol_version":protocol,"result":"error","error":{"code":"internal","message":"blob rejected permanently","retryable":False}}), flush=True)
            continue
        data = b"\x89PNG\r\n\x1a\nrecovered" if method == "read_attachment" else b"recovered-file"
        if filename == "continued": data = b"continued"
        payload = {"type":"attachment_data","data":{"data":base64.b64encode(data).decode()}}
    elif method == "begin_live_baseline":
        print(json.dumps({"request_id":req["request_id"],"protocol_version":protocol,"result":"ok","payload":{"type":"live_baseline_waiting","data":{"blockers":["held-turn"]}}}), flush=True)
        snapshot = b" " * 70000 + b"invalid-json"
        payload = {"type":"live_baseline_started","data":{"total_bytes":len(snapshot)}}
    elif method == "read_live_baseline_page":
        assert snapshot is not None
        offset = params["offset"]
        end = min(offset + 35000, len(snapshot))
        payload = {"type":"live_baseline_page","data":{"page":{"offset":offset,"next_offset":end,"complete":end==len(snapshot),"chunk":base64.b64encode(snapshot[offset:end]).decode()}}}
    else: raise AssertionError(method)
    print(json.dumps({"request_id":req["request_id"],"protocol_version":protocol,"result":"ok","payload":payload}), flush=True)
"#;
    let local = CommandSpec::new(
        "python3",
        [
            "-c".to_string(),
            script.into(),
            SESSION_ID.into(),
            log.display().to_string(),
            RELAY_PROTOCOL_VERSION.to_string(),
        ],
    );
    let target = mj_core::targets::SshTarget {
        destination: "bulk-fixture".into(),
        ssh_args: Vec::new(),
    };
    let control = local
        .clone()
        .ssh_session(&target, mj_core::targets::SshTrafficClass::Control);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut client = RelayClient::connect(&control, SESSION_ID).await.unwrap();
            let bulk = client.bulk_spec.as_ref().unwrap();
            assert_eq!(bulk.args, control.args);
            assert_eq!(
                bulk.ssh_session.as_ref().unwrap().class,
                mj_core::targets::SshTrafficClass::Bulk
            );
            client.install_skills(&vec![b'x'; 70000]).await.unwrap();
            let bytes = b"\x89PNG\r\n\x1a\nimage";
            let reference =
                mj_core::attachment::AttachmentRef::new(bytes, "image/png".into(), 1, 1).unwrap();
            mj_core::attachment::AttachmentStore::controller(SESSION_ID)
                .unwrap()
                .install(&reference, bytes)
                .unwrap();
            client.ensure_attachment(&reference).await.unwrap();
            let file = mj_core::attachment::FileAttachmentRef::new(
                b"file",
                "test.txt",
                "text/plain".into(),
            )
            .unwrap();
            mj_core::attachment::AttachmentStore::controller(SESSION_ID)
                .unwrap()
                .install_file(&file, b"file")
                .unwrap();
            client
                .submit(
                    "batch",
                    RelayCommand::Prompt {
                        prompt: vec![
                            reference.content_block(),
                            file.content_block(),
                            reference.content_block(),
                            file.content_block(),
                        ],
                    },
                )
                .await
                .unwrap();

            let recovered_image = mj_core::attachment::AttachmentRef::new(
                b"\x89PNG\r\n\x1a\nrecovered",
                "image/png".into(),
                1,
                1,
            )
            .unwrap();
            let recovered_file = mj_core::attachment::FileAttachmentRef::new(
                b"recovered-file",
                "recovered.txt",
                "text/plain".into(),
            )
            .unwrap();
            client
                .cache_attachments(
                    &[
                        recovered_image.clone(),
                        reference.clone(),
                        recovered_image.clone(),
                    ],
                    &[recovered_file.clone(), file.clone(), recovered_file.clone()],
                )
                .await
                .unwrap();
            let store = mj_core::attachment::AttachmentStore::controller(SESSION_ID).unwrap();
            assert!(store.contains(&recovered_image).unwrap());
            assert!(store.contains_file(&recovered_file).unwrap());
            // A second pass with everything cached must not open another proxy.
            client
                .cache_attachments(&[recovered_image], &[recovered_file])
                .await
                .unwrap();
            let (progress, observed) = tokio::sync::watch::channel(None);
            client.set_adoption_wait_progress(Some(progress));
            let error = client.live_session_baseline().await.unwrap_err();
            assert_eq!(*observed.borrow(), Some(vec!["held-turn".to_string()]));
            assert!(
                format!("{error:#}").contains("decode live recovery baseline"),
                "{error:#}"
            );
            let error = client.install_skills(b"fail").await.unwrap_err();
            assert!(!RelayTransportDead::marks(&error));
            assert!(!RelayTransportDead::marks_failed_handshake(&error));
            assert!(format!("{error:#}").contains("bulk relay connection failed"));
            let rejected = mj_core::attachment::FileAttachmentRef::new(
                b"reject",
                "reject",
                "text/plain".into(),
            )
            .unwrap();
            let continued = mj_core::attachment::FileAttachmentRef::new(
                b"continued",
                "continued",
                "text/plain".into(),
            )
            .unwrap();
            client
                .cache_attachments(&[], &[rejected.clone(), continued.clone()])
                .await
                .unwrap();
            assert!(!store.contains_file(&rejected).unwrap());
            assert!(store.contains_file(&continued).unwrap());
            assert!(
                captured
                    .at(tracing::Level::WARN)
                    .iter()
                    .any(|line| line.contains("could not cache file attachment during recovery"))
            );
            let disconnected = mj_core::attachment::FileAttachmentRef::new(
                b"disconnect",
                "disconnect",
                "text/plain".into(),
            )
            .unwrap();
            let warnings_before = captured.at(tracing::Level::WARN).len();
            let skipped = mj_core::attachment::FileAttachmentRef::new(
                b"skip-after-disconnect",
                "skip-after-disconnect",
                "text/plain".into(),
            )
            .unwrap();
            client
                .cache_attachments(&[], &[disconnected.clone(), skipped])
                .await
                .unwrap();
            let warnings = captured.at(tracing::Level::WARN);
            assert_eq!(warnings.len(), warnings_before + 1);
            assert!(
                warnings
                    .last()
                    .unwrap()
                    .contains("could not cache attachments during recovery")
            );
            assert!(
                warnings
                    .last()
                    .unwrap()
                    .contains("bulk relay connection failed")
            );
            assert!(client.abandoned.is_none());
            client.skills_state().await.unwrap();
            client.bulk_spec = Some(CommandSpec::new(
                "sh",
                ["-c", "echo bulk-handshake-diagnostic >&2; exit 1"],
            ));
            let error = client.install_skills(b"connect failure").await.unwrap_err();
            assert!(!RelayTransportDead::marks(&error));
            assert!(!RelayTransportDead::marks_failed_handshake(&error));
            assert!(format!("{error:#}").contains("bulk-handshake-diagnostic"));
            let warnings_before = captured.at(tracing::Level::WARN).len();
            client
                .cache_attachments(&[], &[disconnected])
                .await
                .unwrap();
            let warnings = captured.at(tracing::Level::WARN);
            assert_eq!(warnings.len(), warnings_before + 1);
            assert!(
                warnings
                    .last()
                    .unwrap()
                    .contains("could not cache attachments during recovery")
            );
            assert!(
                warnings
                    .last()
                    .unwrap()
                    .contains("bulk-handshake-diagnostic")
            );
            assert!(client.abandoned.is_none());
            client.skills_state().await.unwrap();
            client.detach().await.unwrap();
            let mut client = RelayClient::connect(&local, SESSION_ID).await.unwrap();
            assert!(client.bulk_spec.is_none());
            client.install_skills(b"local").await.unwrap();
            client.skills_state().await.unwrap();
            client.detach().await.unwrap();
        });
    let text = std::fs::read_to_string(log).unwrap();
    let entries: Vec<_> = text
        .lines()
        .map(|line| line.split_once(' ').unwrap())
        .collect();
    let control_pid = entries[0].0;
    assert!(
        entries
            .iter()
            .filter(|(_, method)| matches!(*method, "attachment_present" | "skills_state"))
            .take(3)
            .all(|(pid, _)| *pid == control_pid)
    );
    assert!(
        entries
            .iter()
            .filter(|(_, method)| matches!(
                *method,
                "install_attachment" | "begin_live_baseline" | "read_live_baseline_page"
            ))
            .all(|(pid, _)| *pid != control_pid)
    );
    let upload_pids: Vec<_> = entries
        .iter()
        .filter(|(_, method)| matches!(*method, "install_attachment" | "install_file_attachment"))
        .map(|(pid, _)| *pid)
        .collect();
    assert_eq!(upload_pids.len(), 3);
    assert_eq!(
        upload_pids[1], upload_pids[2],
        "one Bulk connection for the prompt"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, method)| *method == "attachment_present")
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, method)| *method == "file_attachment_present")
            .count(),
        1
    );
    let downloads: Vec<_> = entries
        .iter()
        .filter(|(_, method)| matches!(*method, "read_attachment" | "read_file_attachment"))
        .collect();
    assert_eq!(downloads.len(), 5);
    assert_eq!(
        downloads[0].0, downloads[1].0,
        "one Bulk connection for recovery"
    );
    assert_ne!(downloads[0].0, control_pid);
    let snapshot_pids: std::collections::BTreeSet<_> = entries
        .iter()
        .filter(|(_, method)| matches!(*method, "begin_live_baseline" | "read_live_baseline_page"))
        .map(|(pid, _)| pid)
        .collect();
    assert_eq!(snapshot_pids.len(), 1);
    let local_entries = &entries[entries.len() - 3..];
    assert!(
        local_entries
            .iter()
            .all(|(pid, _)| *pid == local_entries[0].0)
    );
}

#[tokio::test]
async fn pipeline_reads_preserve_rejections_and_reuse_sequential_legacy_stream() {
    let (controller, worker) = tokio::io::duplex(65536);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    client.protocol_version = 1;
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut reader = BufReader::new(reader);
        // Do not answer until both requests arrive: stop-and-wait deadlocks.
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            requests.push(serde_json::from_str::<RelayRequestEnvelope>(&line).unwrap());
        }
        for (i, request) in requests.into_iter().enumerate() {
            let body = if i == 0 {
                RelayResponseBody::Error {
                    error: mj_core::relay::relay_protocol_error(
                        RelayErrorCode::Internal,
                        "one read failed",
                        false,
                        None,
                    ),
                }
            } else {
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::AttachmentPresent { present: true },
                }
            };
            let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: 1,
                body,
            })
            .unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
        }
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
        let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
            request_id: request.request_id,
            protocol_version: 1,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::AttachmentPresent { present: false },
            },
        })
        .unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
    });
    let mut replies = client
        .call_many(vec![RelayRequest::Status, RelayRequest::Status])
        .await
        .unwrap()
        .into_iter();
    assert!(
        replies
            .next()
            .unwrap()
            .unwrap_err()
            .downcast_ref::<RelayRejected>()
            .is_some()
    );
    assert!(matches!(
        replies.next().unwrap().unwrap(),
        RelayResponsePayload::AttachmentPresent { present: true }
    ));
    assert!(client.abandoned.is_none());
    client.call(RelayRequest::Status).await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn pipeline_refuses_writes_before_wire_and_abandons_on_eof() {
    let (controller, worker) = tokio::io::duplex(65536);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let next_request = client.next_request;
    assert!(
        client
            .call_many(vec![
                RelayRequest::Status,
                RelayRequest::Acknowledge {
                    through_ordinal: 1,
                    through_digest: "a".repeat(64),
                }
            ])
            .await
            .is_err()
    );
    assert_eq!(client.next_request, next_request);
    assert!(client.abandoned.is_none());
    client.protocol_version = 1;
    assert!(
        client
            .call_many(vec![RelayRequest::CpuUsage])
            .await
            .is_err()
    );
    assert_eq!(client.next_request, next_request);
    assert!(client.abandoned.is_none());
    drop(worker);
    assert!(client.call_many(vec![RelayRequest::Status]).await.is_err());
    assert!(client.abandoned.is_some());
    assert!(client.call_many(vec![RelayRequest::Status]).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn pipeline_abandons_on_shared_deadline_or_crossed_response() {
    for crossed in [false, true] {
        let (controller, worker) = tokio::io::duplex(65536);
        let mut client = duplex_relay_client(controller, SESSION_ID);
        client.request_timeout = Duration::from_secs(1);
        let peer = mj_core::runtime::spawn(async move {
            let (reader, mut writer) = tokio::io::split(worker);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            if crossed {
                let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
                    request_id: "wrong".into(),
                    protocol_version: request.protocol_version,
                    body: RelayResponseBody::Ok {
                        payload: RelayResponsePayload::AttachmentPresent { present: true },
                    },
                })
                .unwrap();
                bytes.push(b'\n');
                writer.write_all(&bytes).await.unwrap();
            }
            std::future::pending::<()>().await;
        });
        let error = client
            .call_many(vec![RelayRequest::Status, RelayRequest::Status])
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(if crossed { "mismatch" } else { "timed out" }));
        assert!(client.abandoned.is_some());
        assert!(client.call(RelayRequest::Status).await.is_err());
        peer.abort();
        assert!(peer.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn submit_pipelines_deduplicated_image_and_file_presence_before_submit() {
    let (controller, worker) = tokio::io::duplex(65536);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let image = mj_core::attachment::AttachmentRef::new(
        b"\x89PNG\r\n\x1a\nimage",
        "image/png".into(),
        1,
        1,
    )
    .unwrap();
    let file =
        mj_core::attachment::FileAttachmentRef::new(b"file", "a.txt", "text/plain".into()).unwrap();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut reader = BufReader::new(reader);
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            requests.push(serde_json::from_str::<RelayRequestEnvelope>(&line).unwrap());
        }
        assert!(matches!(
            requests[0].request,
            RelayRequest::AttachmentPresent { .. }
        ));
        assert!(matches!(
            requests[1].request,
            RelayRequest::FileAttachmentPresent { .. }
        ));
        for request in requests {
            let mut bytes = serde_json::to_vec(&RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: request.protocol_version,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::AttachmentPresent { present: true },
                },
            })
            .unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
        }
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(request.request, RelayRequest::Submit { .. }));
        // The submission's application reply is irrelevant to presence ordering.
        drop(writer);
    });
    assert!(
        client
            .submit(
                "images",
                RelayCommand::Prompt {
                    prompt: vec![
                        image.content_block(),
                        file.content_block(),
                        image.content_block(),
                        file.content_block()
                    ]
                }
            )
            .await
            .is_err()
    );
    peer.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn call_many_reads_real_worker_unix_server_in_order() {
    let root = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(root.path(), SESSION_ID, "test").unwrap();
    durable
        .record_observation(RelayObservation::Closed)
        .unwrap();
    drop(durable);
    let config: mj_core::worker_launch::WorkerLaunchConfig =
        serde_json::from_value(serde_json::json!({
            "session_id": SESSION_ID, "harness": "codex", "harness_home": root.path(),
            "bridge_command": "/usr/bin/false", "bridge_args": [], "environment": {},
            "cwd": root.path(), "execution_policy": true
        }))
        .unwrap();
    let daemon = mj_core::runtime::spawn(mj_worker::worker_runtime::run_daemon(
        root.path().to_owned(),
        config,
    ));
    let stream = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match tokio::net::UnixStream::connect(root.path().join("control.sock")).await {
                Ok(stream) => break stream,
                Err(_) => {
                    assert!(!daemon.is_finished(), "real worker exited before accepting");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    })
    .await
    .unwrap();
    let (controller, _unused) = tokio::io::duplex(65536);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let (reader, writer) = stream.into_split();
    client.input = Some(Box::new(writer));
    client.output = BufReader::new(Box::new(reader));
    let mut replies = client
        .call_many(vec![RelayRequest::Status, RelayRequest::HistoryRequests])
        .await
        .unwrap()
        .into_iter();
    assert!(
        matches!(replies.next().unwrap().unwrap(), RelayResponsePayload::Status(state) if state.execution == mj_core::relay::RelayExecutionState::Closed)
    );
    assert!(
        matches!(replies.next().unwrap().unwrap(), RelayResponsePayload::HistoryRequests { requests } if requests.is_empty())
    );
    client.status().await.unwrap();
    drop(client);
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn pipeline_drains_replies_while_feeding_more_than_pipe_capacity() {
    let (controller, worker) = tokio::io::duplex(4096);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    let reference = mj_core::attachment::AttachmentRef::new(
        b"\x89PNG\r\n\x1a\nimage",
        "image/png".into(),
        1,
        1,
    )
    .unwrap();
    let requests = vec![RelayRequest::AttachmentPresent { reference }; 1500];
    let count = requests.len();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut reader = BufReader::new(reader);
        let mut request_bytes = 0;
        let mut response_bytes = 0;
        for _ in 0..count {
            let mut line = String::new();
            request_bytes += reader.read_line(&mut line).await.unwrap();
            let request: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            let mut response = serde_json::to_vec(&RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: request.protocol_version,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::AttachmentPresent { present: true },
                },
            })
            .unwrap();
            response.push(b'\n');
            response_bytes += response.len();
            // Match the worker: reply fully before reading its next request.
            writer.write_all(&response).await.unwrap();
        }
        assert!(request_bytes > 64 * 1024);
        assert!(response_bytes > 64 * 1024);
    });
    let replies = tokio::time::timeout(Duration::from_secs(10), client.call_many(requests))
        .await
        .expect("pipelined traffic must not deadlock bounded pipes")
        .unwrap();
    assert_eq!(replies.len(), count);
    assert!(replies.into_iter().all(|reply| matches!(
        reply.unwrap(),
        RelayResponsePayload::AttachmentPresent { present: true }
    )));
    peer.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn pipeline_observation_keeps_its_deadline_before_a_long_attach_slot() {
    let (controller, worker) = tokio::io::duplex(4096);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    client.request_timeout = Duration::from_secs(1);
    let (seen, received) = tokio::sync::oneshot::channel();
    let peer = mj_core::runtime::spawn(async move {
        let (reader, _writer) = tokio::io::split(worker);
        let mut lines = BufReader::new(reader).lines();
        let line = lines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<RelayRequestEnvelope>(&line)
                .unwrap()
                .request,
            RelayRequest::CpuUsage
        ));
        seen.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let started = tokio::time::Instant::now();
    let work = mj_core::runtime::spawn(async move {
        let result = client
            .call_many(vec![
                RelayRequest::CpuUsage,
                RelayRequest::Attach {
                    after_ordinal: 0,
                    after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
                    known_state_digest: None,
                },
            ])
            .await;
        (client, result)
    });
    received.await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    let (client, result) = work.await.unwrap();
    assert!(format!("{:#}", result.unwrap_err()).contains("cpu_usage timed out after 1 seconds"));
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert!(client.abandoned.is_some());
    peer.abort();
    assert!(peer.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn readiness_long_poll_keeps_the_worker_hold_in_its_deadline() {
    use crate::readiness::NativeSessionProbe;
    let (controller, worker) = tokio::io::duplex(4096);
    let mut client = duplex_relay_client(controller, SESSION_ID);
    client.request_timeout = Duration::from_secs(1);
    let peer = mj_core::runtime::spawn(async move {
        let (reader, mut writer) = tokio::io::split(worker);
        let mut lines = BufReader::new(reader).lines();
        let request: RelayRequestEnvelope =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(matches!(
            request.request,
            RelayRequest::WaitForChange { .. }
        ));
        tokio::time::sleep(mj_core::relay::RELAY_CHANGE_WAIT_BOUND).await;
        let mut frame = serde_json::to_vec(&RelayResponseEnvelope {
            request_id: request.request_id,
            protocol_version: request.protocol_version,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::ChangeObserved {
                    latest_ordinal: 0,
                    state_digest: "same".into(),
                    subagent_queue_changed: false,
                    guardian_queue_changed: false,
                },
            },
        })
        .unwrap();
        frame.push(b'\n');
        writer.write_all(&frame).await.unwrap();
    });
    assert!(
        client
            .wait_readiness_change((0, "same".into()), std::future::pending())
            .await
            .unwrap()
    );
    assert!(client.abandoned.is_none());
    peer.await.unwrap();
}
