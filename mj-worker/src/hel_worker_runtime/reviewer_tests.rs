//! The reviewer sidecar against a real harness process.
//!
//! These drive `ReviewerSidecar` through a scripted ACP bridge spawned as an
//! actual child, so the pipes, the process teardown and the reviewer's own
//! durable relay are the real ones rather than in-memory stand-ins.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions,
};

use super::reviewer::{ReviewerPlacement, ReviewerSidecar};
use super::{ReviewerLaunchConfig, unix};
use hel::hel_config::{ExecutionPolicy, HarnessKind};
use hel::hel_worker::{
    RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayCommand, RelayEvent, RelayObservation,
    RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponsePayload, ReviewerRequest,
};

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
/// Comfortably over the 64KB pipe buffer, so a payload that only works
/// because it fits in one buffer fails here.
const LARGE_BYTES: usize = 200 * 1024;

/// A scripted ACP bridge. It answers `initialize`, `session/new`,
/// `session/set_config_option` and `session/prompt`, echoing option lists the
/// test wrote as JSON so the wire shapes are the crate's own.
///
/// The reviewer launches its harness as `<worker_executable> worker
/// acp-supervisor --spec <path>`, so the script ignores its arguments and
/// speaks the protocol on stdio directly.
fn bridge_script(directory: &Path) -> PathBuf {
    let path = directory.join("reviewer-bridge.py");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json, os, signal, sys

here = os.path.dirname(os.path.abspath(__file__))

def load(name, default=None):
    path = os.path.join(here, name)
    if not os.path.exists(path):
        return default
    with open(path) as handle:
        return json.load(handle)

def touch(name):
    open(os.path.join(here, name), "w").close()

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

# A kill leaves this behind. Hel's graceful path should never need it: the
# runtime closes stdin and the harness exits on its own.
signal.signal(signal.SIGTERM, lambda *_: (touch("harness-terminated"), sys.exit(0)))

with open(os.path.join(here, "harness-pid"), "w") as handle:
    handle.write(str(os.getpid()))
prompts = 0
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        write({"jsonrpc": "2.0", "id": ident, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": True},
        }})
    elif method in ("session/new", "session/load"):
        with open(os.path.join(here, "session-method"), "a") as handle:
            handle.write(method + "\n")
        write({"jsonrpc": "2.0", "id": ident, "result": {
            "sessionId": "reviewer-native",
            "configOptions": load("options.json", []),
        }})
    elif method == "session/set_config_option":
        value = request["params"]["value"]
        if os.path.exists(os.path.join(here, "reject-" + value)):
            write({"jsonrpc": "2.0", "id": ident, "error": {
                "code": -32602,
                "message": "unsupported model " + value,
            }})
            continue
        with open(os.path.join(here, "applied"), "a") as handle:
            handle.write("%s=%s\n" % (request["params"]["configId"], value))
        write({"jsonrpc": "2.0", "id": ident, "result": {
            "configOptions": load("options-%s.json" % value, load("options.json", [])),
        }})
    elif method == "session/prompt":
        prompts += 1
        if os.path.exists(os.path.join(here, "ask-form")):
            # Ask the client a question, and answer nothing until it replies.
            write({"jsonrpc": "2.0", "id": "form-1", "method": "elicitation/create",
                   "params": {
                       "sessionId": "reviewer-native",
                       "mode": "form",
                       "message": "Which branch should I compare against?",
                       "requestedSchema": {
                           "type": "object",
                           "title": "Reviewer question",
                           "properties": {},
                       },
                   }})
            for reply in sys.stdin:
                reply = reply.strip()
                if not reply:
                    continue
                answered = json.loads(reply)
                if answered.get("id") == "form-1":
                    break
        text = "".join(
            block.get("text", "")
            for block in request["params"].get("prompt", [])
        )
        with open(os.path.join(here, "prompt-%d" % prompts), "w") as handle:
            handle.write(text)
        answer = load("answer.json", ["reviewed"])
        for chunk in answer:
            write({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "reviewer-native",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": chunk},
                },
            }})
        write({"jsonrpc": "2.0", "id": ident, "result": {"stopReason": "end_turn"}})
    elif ident is not None:
        write({"jsonrpc": "2.0", "id": ident, "result": {}})

# Reached only when the runtime closed this harness's stdin, which is the
# shutdown that terminates the bridge's process group in production.
touch("harness-stdin-closed")
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn select_option(id: &str, current: &str, values: &[&str]) -> SessionConfigOption {
    SessionConfigOption::select(
        id.to_owned(),
        id.to_owned(),
        current.to_owned(),
        SessionConfigSelectOptions::Ungrouped(
            values
                .iter()
                .map(|value| {
                    SessionConfigSelectOption::new((*value).to_owned(), (*value).to_owned())
                })
                .collect(),
        ),
    )
    .category(if id == "model" {
        SessionConfigOptionCategory::Model
    } else {
        SessionConfigOptionCategory::ThoughtLevel
    })
}

fn write_options(directory: &Path, name: &str, options: &[SessionConfigOption]) {
    std::fs::write(directory.join(name), serde_json::to_vec(options).unwrap()).unwrap();
}

struct Fixture {
    _temp: tempfile::TempDir,
    worker_root: PathBuf,
    /// Where the bridge script keeps its markers, which is also the reviewer's
    /// staged profile directory.
    profile_home: PathBuf,
    sidecar: ReviewerSidecar,
}

impl Fixture {
    /// Builds a worker root with a staged reviewer profile and a scripted
    /// harness. `stage` controls whether the profile directory exists, since
    /// starting without one has to fail.
    fn new(stage: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let worker_root = temp.path().join("worker");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let profile_home = worker_root.join("reviewer").join("profile");
        if stage {
            std::fs::create_dir_all(&profile_home).unwrap();
        }
        let bridge = bridge_script(temp.path());
        let sidecar = ReviewerSidecar::new(ReviewerPlacement {
            worker_root: worker_root.clone(),
            session_id: SESSION_ID.to_owned(),
            cwd: workspace,
            additional_directories: Vec::new(),
            worker_executable: bridge,
            harness_runtime: hel::hel_worker_launch::HarnessRuntimePolicy::Ambient,
        });
        Self {
            _temp: temp,
            worker_root,
            profile_home,
            sidecar,
        }
    }

    /// The bridge script writes its markers beside itself, one directory above
    /// the staged profile.
    fn script_directory(&self) -> PathBuf {
        self.worker_root
            .parent()
            .expect("the worker root has a parent")
            .to_owned()
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.script_directory().join(name)
    }

    fn stage_generation(&self, generation: u64) {
        if generation == 0 {
            return;
        }
        let destination = self
            .worker_root
            .join("reviewer")
            .join(format!("profile-{generation}"));
        copy_tree_for_test(&self.profile_home, &destination);
    }

    /// The scripted harness's process id, recorded when it started.
    fn harness_pid(&self) -> i32 {
        std::fs::read_to_string(self.marker("harness-pid"))
            .expect("the harness records its process id when it starts")
            .trim()
            .parse()
            .expect("the recorded process id is a number")
    }

    async fn request(&mut self, request: ReviewerRequest) -> RelayResponseBody {
        self.request_as(None, request).await
    }

    /// Drives one named reviewing role, which is how the extended review tier
    /// runs its supervisor and lanes beside the default reviewer.
    async fn request_as(
        &mut self,
        role: Option<&str>,
        request: ReviewerRequest,
    ) -> RelayResponseBody {
        let envelope = RelayRequestEnvelope {
            request_id: "test".to_owned(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Reviewer {
                role: role.map(str::to_owned),
                request: request.clone(),
            },
        };
        self.sidecar
            .handle(envelope, role.map(str::to_owned), request)
            .await
            .body
    }

    async fn start(&mut self, config: ReviewerLaunchConfig) -> RelayResponseBody {
        self.request(ReviewerRequest::Start {
            config: Box::new(config),
        })
        .await
    }

    /// Every event the reviewer has journaled, read the way a controller
    /// reads it. Opening a second relay on the same journal would recover it
    /// underneath the live one, so the attach path is the only safe reader.
    async fn reviewer_events(&mut self) -> Vec<RelayEvent> {
        let body = self
            .request(ReviewerRequest::Attach {
                after_ordinal: 0,
                after_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            })
            .await;
        match body {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Attached { events, .. },
            } => events,
            other => panic!("expected an attach page, got {other:?}"),
        }
    }

    /// Polls the reviewer's journal until `ready` accepts it.
    async fn await_events(
        &mut self,
        timeout: Duration,
        ready: impl Fn(&[RelayEvent]) -> bool,
    ) -> Vec<RelayEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let events = self.reviewer_events().await;
            if ready(&events) {
                return events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reviewer did not reach the expected state in {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn copy_tree_for_test(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree_for_test(&source, &destination);
        } else {
            std::fs::copy(source, destination).unwrap();
        }
    }
}

fn config(generation: u64) -> ReviewerLaunchConfig {
    ReviewerLaunchConfig {
        profile_id: "reviewer-profile".into(),
        harness: HarnessKind::Kimi,
        // The sidecar spawns the worker executable, which the fixture points
        // at the scripted bridge, so this command is never reached.
        bridge_command: "/bin/false".into(),
        bridge_args: Vec::new(),
        environment: Default::default(),
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        model: None,
        effort: None,
        generation,
        mcp_servers: Vec::new(),
    }
}

fn started_options(body: &RelayResponseBody) -> (&Vec<SessionConfigOption>, bool) {
    let RelayResponseBody::Ok {
        payload:
            RelayResponsePayload::ReviewerStarted {
                config_options,
                reused,
                ..
            },
    } = body
    else {
        panic!("expected a started reviewer, got {body:?}");
    };
    (config_options, *reused)
}

fn error_message(body: &RelayResponseBody) -> String {
    match body {
        RelayResponseBody::Error { error } => error.message.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_reviewer_cannot_start_before_its_profile_is_staged() {
    let mut fixture = Fixture::new(false);
    let body = fixture.start(config(0)).await;
    assert!(
        error_message(&body).contains("has not been staged"),
        "unexpected error: {body:?}"
    );
    assert!(
        !fixture.marker("harness-started").exists(),
        "no harness may run without a staged profile"
    );
}

#[tokio::test]
async fn starting_opens_a_native_session_and_reports_what_the_harness_advertises() {
    let mut fixture = Fixture::new(true);
    write_options(
        &fixture.script_directory(),
        "options.json",
        &[select_option("model", "fast", &["fast", "deep"])],
    );

    let body = fixture.start(config(0)).await;
    let (options, reused) = started_options(&body);
    assert!(!reused, "the first start launches a reviewer");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].id.to_string(), "model");
    assert!(process_alive(fixture.harness_pid()));

    // A second start with the same identity reuses the running harness rather
    // than paying for another one.
    let body = fixture.start(config(0)).await;
    let (_, reused) = started_options(&body);
    assert!(reused, "the same configuration reuses the running reviewer");

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_harness_that_advertises_nothing_still_starts() {
    let mut fixture = Fixture::new(true);
    write_options(&fixture.script_directory(), "options.json", &[]);

    let body = fixture.start(config(0)).await;
    let (options, _) = started_options(&body);
    assert!(
        options.is_empty(),
        "a harness with no selectors advertises none"
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn applying_a_model_refreshes_the_advertised_efforts() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.script_directory();
    write_options(
        &directory,
        "options.json",
        &[
            select_option("model", "fast", &["fast", "deep"]),
            select_option("effort", "low", &["low"]),
        ],
    );
    write_options(
        &directory,
        "options-deep.json",
        &[
            select_option("model", "deep", &["fast", "deep"]),
            select_option("effort", "high", &["high", "highest"]),
        ],
    );

    fixture.start(config(0)).await;
    let mut chosen = config(0);
    chosen.model = Some("deep".into());
    let body = fixture.start(chosen).await;
    let (options, reused) = started_options(&body);
    assert!(reused, "choosing a model configures the running reviewer");
    let efforts = hel::hel_acp::session_config_choices(options, "effort")
        .into_iter()
        .map(|choice| choice.value)
        .collect::<Vec<_>>();
    assert_eq!(efforts, vec!["high", "highest"]);
    assert_eq!(
        std::fs::read_to_string(directory.join("applied")).unwrap(),
        "model=deep\n"
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_new_generation_replaces_the_running_reviewer() {
    let mut fixture = Fixture::new(true);
    write_options(&fixture.script_directory(), "options.json", &[]);

    fixture.start(config(0)).await;
    let replaced = fixture.harness_pid();
    assert!(process_alive(replaced));

    fixture.stage_generation(1);
    let body = fixture.start(config(1)).await;
    let (_, reused) = started_options(&body);
    assert!(!reused, "a new generation is a new reviewer");
    assert!(
        !process_alive(replaced),
        "replacing a reviewer must stop the old harness first"
    );
    assert_ne!(
        fixture.harness_pid(),
        replaced,
        "the replacement is a different process"
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_new_generation_starts_a_fresh_native_conversation() {
    let mut fixture = Fixture::new(true);
    fixture.start(config(0)).await;
    fixture.sidecar.pause_all().await;

    fixture.stage_generation(1);
    fixture.start(config(1)).await;

    let methods = std::fs::read_to_string(fixture.marker("session-method")).unwrap();
    assert_eq!(
        methods.lines().collect::<Vec<_>>(),
        vec!["session/new", "session/new"]
    );
    assert!(
        fixture
            .worker_root
            .join("reviewer")
            .join("relay-archive")
            .exists(),
        "the previous relay is retained outside the live generation"
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_failed_model_start_can_retry_with_a_corrected_model() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.script_directory();
    write_options(
        &directory,
        "options.json",
        &[select_option("model", "opus[1m]", &["opus[1m]"])],
    );
    std::fs::write(directory.join("reject-opus"), b"1").unwrap();

    let mut rejected = config(0);
    rejected.model = Some("opus".into());
    let body = fixture.start(rejected).await;
    assert!(
        matches!(body, RelayResponseBody::Error { .. }),
        "the unsupported first model must fail: {body:?}"
    );
    fixture.sidecar.pause_all().await;

    std::fs::remove_file(directory.join("reject-opus")).unwrap();
    fixture.stage_generation(1);
    let mut corrected = config(1);
    corrected.model = Some("opus[1m]".into());
    let body = fixture.start(corrected).await;
    assert!(
        matches!(body, RelayResponseBody::Ok { .. }),
        "a corrected model must start a new reviewer: {body:?}"
    );
    let methods = std::fs::read_to_string(fixture.marker("session-method")).unwrap();
    assert_eq!(
        methods.lines().collect::<Vec<_>>(),
        vec!["session/new", "session/new"]
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_review_turn_streams_through_the_reviewers_own_relay() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.script_directory();
    write_options(&directory, "options.json", &[]);
    std::fs::write(
        directory.join("answer.json"),
        serde_json::to_vec(&vec!["the plan ", "misses error handling"]).unwrap(),
    )
    .unwrap();

    fixture.start(config(0)).await;
    let accepted = fixture
        .request(ReviewerRequest::Submit {
            command_id: "review-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("critique this plan"),
                )],
            },
        })
        .await;
    assert!(
        matches!(
            accepted,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Accepted { .. }
            }
        ),
        "unexpected submit response: {accepted:?}"
    );

    let events = fixture
        .await_events(Duration::from_secs(30), |events| {
            collected_agent_text(events).contains("misses error handling")
        })
        .await;
    assert_eq!(
        collected_agent_text(&events),
        "the plan misses error handling"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("prompt-1")).unwrap(),
        "critique this plan"
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_review_survives_payloads_larger_than_a_pipe_buffer() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.script_directory();
    write_options(&directory, "options.json", &[]);
    // Both directions exceed the 64KB pipe buffer, so a request written before
    // its response is drained would deadlock rather than pass.
    let answer = "y".repeat(LARGE_BYTES);
    std::fs::write(
        directory.join("answer.json"),
        serde_json::to_vec(&vec![answer.clone()]).unwrap(),
    )
    .unwrap();
    let plan = "x".repeat(LARGE_BYTES);

    fixture.start(config(0)).await;
    fixture
        .request(ReviewerRequest::Submit {
            command_id: "review-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new(plan.clone()),
                )],
            },
        })
        .await;

    let events = fixture
        .await_events(Duration::from_secs(60), |events| {
            collected_agent_text(events).len() >= LARGE_BYTES
        })
        .await;
    assert_eq!(collected_agent_text(&events), answer);
    assert_eq!(
        std::fs::read_to_string(directory.join("prompt-1")).unwrap(),
        plan
    );

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_reviewer_form_is_answered_on_the_connection() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.script_directory();
    write_options(&directory, "options.json", &[]);
    // The harness asks a question mid-turn and only finishes once it is
    // answered, so an unanswered form would stall this test rather than pass.
    std::fs::write(directory.join("ask-form"), b"1").unwrap();

    fixture.start(config(0)).await;
    fixture
        .request(ReviewerRequest::Submit {
            command_id: "review-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("critique this plan"),
                )],
            },
        })
        .await;

    // Wait for the reviewer to journal the form it is waiting on.
    let events = fixture
        .await_events(Duration::from_secs(30), |events| {
            events.iter().any(|event| {
                matches!(
                    &event.observation,
                    RelayObservation::ElicitationRequested { .. }
                )
            })
        })
        .await;
    let elicitation_id = events
        .iter()
        .find_map(|event| match &event.observation {
            RelayObservation::ElicitationRequested { request } => Some(request.id.clone()),
            _ => None,
        })
        .expect("the reviewer journals the form it waits on");

    let body = fixture
        .request(ReviewerRequest::RespondElicitation {
            elicitation_id: elicitation_id.clone(),
            response: hel::hel_elicitation::ElicitationResponse::Decline,
        })
        .await;
    assert!(
        matches!(
            &body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::ElicitationResolved { elicitation_id: resolved }
            } if *resolved == elicitation_id
        ),
        "unexpected form answer response: {body:?}"
    );

    // Answering unblocks the turn, which is the whole point.
    fixture
        .await_events(Duration::from_secs(30), |events| {
            collected_agent_text(events).contains("reviewed")
        })
        .await;

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn pausing_stops_the_harness_process_group_and_keeps_the_journal() {
    let mut fixture = Fixture::new(true);
    write_options(&fixture.script_directory(), "options.json", &[]);

    fixture.start(config(0)).await;
    let events_before = fixture.reviewer_events().await.len();
    assert!(events_before > 0);
    let harness = fixture.harness_pid();

    let body = fixture.request(ReviewerRequest::Pause).await;
    assert!(
        matches!(
            body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::ReviewerPaused
            }
        ),
        "unexpected pause response: {body:?}"
    );
    assert!(!process_alive(harness), "pausing must stop the harness");
    // Pausing must reach the harness by closing its stdin, which is what lets
    // the supervisor terminate the bridge's process group in production.
    // Killing the runtime instead would strand that group.
    assert!(
        fixture.marker("harness-stdin-closed").exists(),
        "the reviewer must be shut down gracefully, not killed"
    );
    assert!(
        !fixture.marker("harness-terminated").exists(),
        "a graceful shutdown needs no signal"
    );

    // The conversation is retained, so the next review reloads it.
    let events = fixture.reviewer_events().await;
    assert!(events.len() >= events_before);
    assert!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::SessionOpened {
                native_session_id, ..
            } if native_session_id == "reviewer-native"
        )),
        "the reviewer's native session identity must survive a pause"
    );
    assert!(fixture.profile_home.is_dir(), "the staged profile is kept");
}

#[tokio::test]
async fn a_resumed_reviewer_reloads_its_native_session() {
    let mut fixture = Fixture::new(true);
    write_options(&fixture.script_directory(), "options.json", &[]);

    fixture.start(config(0)).await;
    fixture.sidecar.pause_all().await;
    fixture.start(config(0)).await;

    let resumes = fixture
        .reviewer_events()
        .await
        .into_iter()
        .filter(|event| {
            matches!(
                &event.observation,
                RelayObservation::SessionOpened { resumed, .. } if *resumed
            )
        })
        .count();
    assert_eq!(resumes, 1, "the second start reloads rather than restarts");

    fixture.sidecar.pause_all().await;
}

#[tokio::test]
async fn a_harness_that_never_answers_reports_why_instead_of_hanging() {
    let mut fixture = Fixture::new(true);
    // A harness that exits at once cannot open a session. The failure has to
    // come back as an error rather than as a wait that never ends.
    let script = fixture.script_directory().join("reviewer-bridge.py");
    std::fs::write(&script, "#!/usr/bin/env python3\nimport sys\nsys.exit(3)\n").unwrap();

    let body = fixture.start(config(0)).await;
    assert!(
        matches!(body, RelayResponseBody::Error { .. }),
        "a dead harness must be reported: {body:?}"
    );
}

#[tokio::test]
async fn the_reviewer_keeps_its_files_inside_the_primary_worker_root() {
    let mut fixture = Fixture::new(true);
    write_options(&fixture.script_directory(), "options.json", &[]);
    fixture.start(config(0)).await;
    fixture.sidecar.pause_all().await;

    let reviewer_root = fixture.worker_root.join("reviewer");
    assert!(reviewer_root.join("relay-state.json").exists());
    assert!(reviewer_root.join("relay-journal").is_dir());
    assert!(reviewer_root.join("acp-supervisor.json").exists());
    // Nothing the reviewer owns may sit beside the primary's own relay files.
    assert!(!fixture.worker_root.join("relay-state.json").exists());
    assert!(!fixture.worker_root.join("relay-journal").exists());
}

#[test]
fn a_reviewer_request_needs_the_protocol_that_introduced_it() {
    let request = RelayRequest::Reviewer {
        role: None,
        request: ReviewerRequest::Status,
    };
    assert_eq!(request.minimum_protocol(), 6);
    assert!(!request.supported_at(5));
    assert!(request.supported_at(RELAY_PROTOCOL_VERSION));
    assert_eq!(request.method_name(), "reviewer_status");
}

#[test]
fn the_plain_relay_refuses_reviewer_requests() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = hel::hel_worker::DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    let response = relay.handle(RelayRequestEnvelope {
        request_id: "req-1".to_owned(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Reviewer {
            role: None,
            request: ReviewerRequest::Status,
        },
    });
    assert!(
        error_message(&response.body).contains("live relay transport"),
        "unexpected response: {response:?}"
    );
}

#[test]
fn a_running_reviewer_is_reused_only_for_the_same_profile_and_generation() {
    let base = config(0);
    assert!(base.reusable_for(&config(0)));
    assert!(!base.reusable_for(&config(1)));

    let mut other_profile = config(0);
    other_profile.profile_id = "another".into();
    assert!(!base.reusable_for(&other_profile));

    // Model and effort are applied on the live session, so they never force a
    // restart that would throw the reviewer's conversation away.
    let mut configured = config(0);
    configured.model = Some("deep".into());
    configured.effort = Some("high".into());
    assert!(base.reusable_for(&configured));
}

/// Whether `pid` still names a live process.
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check only; it
    // delivers nothing and cannot affect the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Agent text the reviewer's journal carries, in order.
fn collected_agent_text(events: &[RelayEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.observation {
            RelayObservation::SessionUpdate { update } => Some(update.as_ref()),
            _ => None,
        })
        .filter_map(|update| {
            let value = serde_json::to_value(update).ok()?;
            (value.get("sessionUpdate")?.as_str()? == "agent_message_chunk")
                .then(|| {
                    value
                        .get("content")?
                        .get("text")?
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten()
        })
        .collect()
}

/// The unused import guard: the sidecar's coordinator lives in `unix`, and a
/// test build that cannot see it would silently stop covering the real one.
#[test]
fn the_sidecar_uses_the_worker_relay_coordinator() {
    let _ = unix::ACP_EVENT_CHANNEL_CAPACITY;
}

#[tokio::test]
async fn two_review_roles_run_side_by_side_with_their_own_homes_and_journals() {
    let mut fixture = Fixture::new(true);
    // A file in the staged profile proves each role gets a copy rather than a
    // shared directory: a second harness writing its session files into the
    // first one's home would corrupt both.
    std::fs::write(fixture.profile_home.join("credentials"), b"token\n").unwrap();

    let body = fixture.start(config(0)).await;
    let (_, reused) = started_options(&body);
    assert!(!reused, "the default role starts its own harness");
    assert!(
        fixture
            .worker_root
            .join("reviewer")
            .join("runtime-profile")
            .join("credentials")
            .exists(),
        "the default role runs from a private copy of the staged profile"
    );

    let body = fixture
        .request_as(
            Some("tests"),
            ReviewerRequest::Start {
                config: Box::new(config(0)),
            },
        )
        .await;
    let (_, reused) = started_options(&body);
    assert!(!reused, "a lane is a different harness, not a reused one");

    let lane_home = fixture
        .worker_root
        .join("reviewer")
        .join("roles")
        .join("tests")
        .join("profile");
    assert!(
        lane_home.join("credentials").exists(),
        "a lane runs from its own copy of the staged profile"
    );
    assert_ne!(
        lane_home, fixture.profile_home,
        "the lane's home is not the staged profile itself"
    );
    assert!(
        fixture
            .worker_root
            .join("reviewer")
            .join("roles")
            .join("tests")
            .join("relay-journal")
            .exists()
            || fixture
                .worker_root
                .join("reviewer")
                .join("roles")
                .join("tests")
                .join("relay-state.json")
                .exists(),
        "the lane journals into its own directory: {:?}",
        std::fs::read_dir(
            fixture
                .worker_root
                .join("reviewer")
                .join("roles")
                .join("tests")
        )
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>()
    );

    // Each role answers its own prompts, and one role's transcript never
    // appears in another's journal.
    let accepted = fixture
        .request(ReviewerRequest::Submit {
            command_id: "review-default".to_owned(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new("default role"),
                )],
            },
        })
        .await;
    assert!(matches!(accepted, RelayResponseBody::Ok { .. }));
    let accepted = fixture
        .request_as(
            Some("tests"),
            ReviewerRequest::Submit {
                command_id: "review-lane".to_owned(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new("lane role"),
                    )],
                },
            },
        )
        .await;
    assert!(matches!(accepted, RelayResponseBody::Ok { .. }));

    let lane_events = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let body = fixture
                .request_as(
                    Some("tests"),
                    ReviewerRequest::Attach {
                        after_ordinal: 0,
                        after_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                    },
                )
                .await;
            let RelayResponseBody::Ok {
                payload: RelayResponsePayload::Attached { events, .. },
            } = body
            else {
                panic!("attach answers with a page of events");
            };
            if events.iter().any(|event| {
                matches!(
                    &event.observation,
                    RelayObservation::CommandCompleted { command_id, .. }
                        if command_id == "review-lane"
                )
            }) {
                break events;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the lane answers its own prompt");
    assert!(
        !lane_events.iter().any(|event| {
            matches!(
                &event.observation,
                RelayObservation::CommandCompleted { command_id, .. }
                    if command_id == "review-default"
            )
        }),
        "the default role's turn stays out of the lane's journal"
    );

    fixture.sidecar.pause_all().await;
}

/// The supervisor's dispatch tool runs as a separate process and reaches its
/// worker over a socket, so the two halves are exercised together here rather
/// than each against a stand-in for the other.
#[tokio::test]
async fn the_dispatch_socket_records_what_the_supervisor_asks_for() {
    use hel::hel_review::lanes::{LaneDispatch, ReviewSubagentRequest};

    let temp = tempfile::tempdir().unwrap();
    let worker_root = temp.path().join("worker");
    std::fs::create_dir_all(&worker_root).unwrap();
    let sidecar = std::sync::Arc::new(ReviewerSidecar::new(ReviewerPlacement {
        worker_root: worker_root.clone(),
        session_id: SESSION_ID.to_owned(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        worker_executable: PathBuf::from("/bin/false"),
        harness_runtime: hel::hel_worker_launch::HarnessRuntimePolicy::Ambient,
    }));
    let _guard = unix::serve_review_dispatch(&worker_root, sidecar.clone()).unwrap();
    let socket = worker_root
        .join("reviewer")
        .join(hel::hel_review::mcp::REVIEW_DISPATCH_SOCKET);

    let dispatch = LaneDispatch {
        reviewers: vec![
            ReviewSubagentRequest {
                agent_type: "tests".to_owned(),
                hypothesis: "the new test cannot fail for the reason it claims".to_owned(),
            },
            ReviewSubagentRequest {
                agent_type: "error_handling".to_owned(),
                hypothesis: "the retry may swallow cancellation".to_owned(),
            },
        ],
    };
    let socket_for_call = socket.clone();
    let dispatch_for_call = dispatch.clone();
    let reply = tokio::task::spawn_blocking(move || {
        hel::hel_review::mcp::send_dispatch(&socket_for_call, &dispatch_for_call)
    })
    .await
    .unwrap()
    .expect("the worker answers a dispatch");
    assert_eq!(reply.started, vec!["tests", "error_handling"]);
    assert_eq!(reply.error, None);

    // A lane already asked for is not queued twice: its report is still
    // coming, and a second copy would double the container's load.
    let socket_for_call = socket.clone();
    let reply = tokio::task::spawn_blocking(move || {
        hel::hel_review::mcp::send_dispatch(&socket_for_call, &dispatch)
    })
    .await
    .unwrap()
    .expect("the worker answers a repeat dispatch");
    assert!(reply.started.is_empty());

    // The controller collects the queue once, and it is empty afterwards.
    let collected = sidecar.take_dispatches();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].agent_type, "tests");
    assert!(sidecar.take_dispatches().is_empty());

    // An invalid dispatch is refused with a message the supervisor can act on.
    let socket_for_call = socket.clone();
    let reply = tokio::task::spawn_blocking(move || {
        hel::hel_review::mcp::send_dispatch(
            &socket_for_call,
            &LaneDispatch {
                reviewers: vec![ReviewSubagentRequest {
                    agent_type: "not_a_lane".to_owned(),
                    hypothesis: "there is no such specialist".to_owned(),
                }],
            },
        )
    })
    .await
    .unwrap()
    .expect("the worker answers an invalid dispatch");
    assert!(reply.started.is_empty());
    assert!(
        reply
            .error
            .as_deref()
            .is_some_and(|error| error.contains("advertised roster")),
        "unexpected reply {reply:?}"
    );
}
