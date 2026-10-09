//! Replays the recorded Jev scenarios in `tests/jev-scenarios/` against the
//! deterministic policy: the action `Verdict::action` takes on the recorded
//! answer, and whether the recorded process facts count as quiet.
//!
//! Each fixture is one real turn from a live session (see
//! `scripts/jev-scenarios-extract.py`). Two things are asserted here. First,
//! the recorded verdict must never produce an action the fixture lists as
//! wrong: that is the safety property, independent of how confident Jev was.
//! Second, when the fixture's runtime facts were recorded, the quiet rule must
//! agree with the fixture's `expected.quiet`, unless the fixture is marked
//! `known_failure: ["quiet"]`, which documents a case the current rule gets
//! wrong until the fix in `.agents/plans/jev-quiet-and-scenario-suite.md`.
//! The live-model half of the suite is `scripts/jev-scenarios-eval.py`.

use std::path::PathBuf;

use mj_core::activity::verdict::{
    STALE_FINISHED_SILENCE, STALE_REPLY_PROBABILITY, TurnEvidence, TurnPhase, TurnVerdict,
    WorkState, stale_finished,
};
use mj_core::activity::{ActivityFacts, InFlightToolCall, quiet_at};
use mj_core::assessment::{Action, Judgment, Reply, Verdict, Work};
use mj_core::relay::RelayExecutionState;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
// Keep the full strict fixture schema even when a policy replay reads only a
// subset of each recorded field.
#[allow(dead_code)]
struct Fixture {
    id: String,
    title: String,
    category: String,
    harness: String,
    source: serde_json::Value,
    facts_known: bool,
    facts: Facts,
    evidence: TurnEvidence,
    /// The answer Jev gave at the time. `None` for turns that predate Jev or
    /// whose classification never resolved; the live replay still scores them.
    recorded_verdict: Option<serde_json::Value>,
    expected: Option<Expected>,
    #[serde(default)]
    known_failure: Vec<String>,
    outcome: Option<serde_json::Value>,
    context: serde_json::Value,
}

/// Process facts the evidence does not carry. `task_settled_s_ago` and
/// `harness_turn_open` are read once the quiet rule learns about them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Facts {
    execution: String,
    harness_turn_open: bool,
    goal_active: bool,
    active_user_shells: usize,
    active_agent_terminals: usize,
    task_settled_s_ago: Option<u64>,
    /// Jev's confident judgment of the leftover processes, when one exists.
    background_needed: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
// These recorded answer fields remain required so fixture shape drift is
// rejected, even though the two replay checks use different axes.
#[allow(dead_code)]
struct Expected {
    failure: String,
    input: String,
    work: String,
    /// What the final reply signals about whether the agent will continue now.
    #[serde(default)]
    reply: Option<String>,
    /// The right answer to the background question, when the evidence lists
    /// background commands.
    #[serde(default)]
    background: Option<String>,
    /// `None` when the runtime facts were not recorded, so quiet is unknown.
    quiet: Option<bool>,
    /// The right Mjolnir action given a correct verdict.
    action: String,
    /// Actions that would have been harmful on this turn.
    #[serde(default)]
    wrong_actions: Vec<String>,
}

fn fixtures() -> Vec<(PathBuf, Fixture)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/jev-scenarios");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let fixture: Fixture = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            (path, fixture)
        })
        .collect()
}

fn action_name(action: Action) -> &'static str {
    match action {
        Action::RetryProvider => "retry_provider",
        Action::RecoverQuota => "recover_quota",
        Action::Continue => "continue",
        Action::AwaitInput => "await_input",
        Action::Finished => "finished",
        Action::Wait => "wait",
        Action::Uncertain => "uncertain",
    }
}

/// The same admission test `apply_turn_assessment` applies before it will
/// let a verdict continue: whole, validated authorization history.
fn authorization_complete(evidence: &TurnEvidence) -> bool {
    evidence.authorization.as_ref().is_some_and(|context| {
        context.authorization_complete
            && !context.final_reply_omitted
            && context.evidence().validate().is_ok()
    })
}

fn activity_facts(fixture: &Fixture) -> ActivityFacts {
    let evidence = &fixture.evidence;
    let running = fixture.facts.execution == "running";
    let now = NOW_MS;
    ActivityFacts {
        execution: if running {
            RelayExecutionState::Running
        } else {
            RelayExecutionState::Idle
        },
        prompt_started_at_ms: running.then_some(now - 60_000),
        harness_turn_started_at_ms: fixture.facts.harness_turn_open.then_some(now - 1_000),
        turn_started_at_ms: running.then_some(now - 60_000),
        queued_commands: evidence.queued_commands,
        tools_in_flight: evidence
            .tools_in_flight
            .iter()
            .enumerate()
            .map(|(index, tool)| InFlightToolCall {
                tool_call_id: format!("tool-{index}"),
                title: Some(tool.title.clone()),
                status: agent_client_protocol::schema::v1::ToolCallStatus::InProgress,
                started_at_ms: now - (tool.running_s as i64) * 1_000,
            })
            .collect(),
        background_started_at_ms: (evidence.background_commands > 0).then_some(now - 120_000),
        background_commands: evidence.background_commands,
        active_user_shells: fixture.facts.active_user_shells,
        active_agent_terminals: fixture.facts.active_agent_terminals,
        goal_active: fixture.facts.goal_active,
        goal_running: fixture.facts.goal_active && running,
        goal_synchronized: true,
        acp_ready: Some(true),
        task_settled_at_ms: fixture
            .facts
            .task_settled_s_ago
            .map(|seconds| now - (seconds as i64) * 1_000),
        background_needed: fixture.facts.background_needed,
        ..ActivityFacts::default()
    }
}

/// The clock every fixture is evaluated at; `activity_facts` places events
/// relative to it.
const NOW_MS: i64 = 1_000_000_000;

#[test]
fn recorded_verdicts_never_produce_a_wrong_action() {
    let mut checked = 0;
    for (path, fixture) in fixtures() {
        let Some(recorded) = fixture.recorded_verdict.as_ref() else {
            continue;
        };
        // Only the current contract has the three-axis answer the policy reads.
        let Ok(verdict) = serde_json::from_value::<Verdict>(recorded.clone()) else {
            continue;
        };
        let expected = fixture.expected.as_ref().unwrap();
        let action = action_name(verdict.action(authorization_complete(&fixture.evidence)));
        let wrong = expected.wrong_actions.iter().any(|name| name == action);
        if fixture.known_failure.iter().any(|name| name == "action") {
            assert!(
                wrong,
                "{}: marked known_failure action but the policy is right now; remove the mark",
                path.display()
            );
        } else {
            assert!(
                !wrong,
                "{}: recorded verdict {recorded} yields {action}, listed as wrong",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no fixture carried a current-contract verdict");
}

#[test]
fn quiet_matches_the_recorded_facts() {
    let mut checked = 0;
    for (path, fixture) in fixtures() {
        if !fixture.facts_known {
            continue;
        }
        let Some(expected_quiet) = fixture.expected.as_ref().and_then(|e| e.quiet) else {
            continue;
        };
        let quiet = quiet_at(&activity_facts(&fixture), NOW_MS).is_yes();
        if fixture.known_failure.iter().any(|name| name == "quiet") {
            assert_ne!(
                quiet,
                expected_quiet,
                "{}: marked known_failure quiet but the rule agrees now; remove the mark",
                path.display()
            );
        } else {
            assert_eq!(
                quiet,
                expected_quiet,
                "{}: quiet rule disagrees with the recorded outcome",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no fixture recorded its runtime facts");
}

fn recorded_turn_verdict(recorded: &serde_json::Value) -> TurnVerdict {
    if let Some(work_state) = recorded
        .get("work_state")
        .and_then(serde_json::Value::as_str)
    {
        let work_state = match work_state {
            "BackgroundWork" => WorkState::BackgroundWork,
            "StillWorking" => WorkState::StillWorking,
            "Finished" => WorkState::Finished,
            _ => WorkState::Unclear,
        };
        let assessment = recorded
            .get("assessment")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value(value.clone())
                    .expect("recorded modern assessment deserializes")
            });
        return TurnVerdict {
            assessment,
            work_state,
            work_state_confidence: recorded["work_state_confidence"]
                .as_f64()
                .expect("recorded work confidence is numeric")
                as f32,
            needs_user_input: recorded["needs_user_input"]
                .as_f64()
                .expect("recorded input confidence is numeric")
                as f32,
            retryable_server_error: recorded["retryable_server_error"]
                .as_f64()
                .map(|probability| probability as f32),
        };
    }
    let response = if recorded.get("answers").is_some() {
        recorded.clone()
    } else {
        json!({ "answers": recorded })
    };
    TurnVerdict::parse(&response).expect("recorded turn verdict parses")
}

fn finished_verdict(work_probability: f64) -> TurnVerdict {
    TurnVerdict::parse(&json!({
        "answers": {
            "failure": {
                "type": "choice", "choice": "none", "confidence": 0.90,
                "probabilities": {
                    "none": 0.90, "transient_provider": 0.10,
                    "quota": 0.0, "other": 0.0, "unclear": 0.0
                }
            },
            "input": {
                "type": "choice", "choice": "none", "confidence": 0.85,
                "probabilities": {
                    "none": 0.85, "redundant_request": 0.15,
                    "required": 0.0, "unclear": 0.0
                }
            },
            "work": {
                "type": "choice", "choice": "finished", "confidence": work_probability,
                "probabilities": {
                    "finished": work_probability,
                    "authorized_unfinished": 1.0 - work_probability,
                    "waiting": 0.0, "unclear": 0.0
                }
            }
        }
    }))
    .expect("synthetic modern finished verdict parses")
}

fn closing_reply_verdict(closing_probability: f64) -> TurnVerdict {
    let mut verdict = finished_verdict(0.79);
    let assessment = verdict
        .assessment
        .as_mut()
        .expect("synthetic modern assessment is present");
    assessment.work.choice = Work::AuthorizedUnfinished;
    assessment.work.confidence = 0.99;
    assessment.work.probabilities = [
        ("finished".into(), 0.01),
        ("authorized_unfinished".into(), 0.99),
        ("waiting".into(), 0.0),
        ("unclear".into(), 0.0),
    ]
    .into();
    assessment.reply = Some(Judgment {
        choice: Reply::Closing,
        confidence: closing_probability as f32,
        probabilities: [
            ("closing".into(), closing_probability),
            ("continuing".into(), 1.0 - closing_probability),
            ("unclear".into(), 0.0),
        ]
        .into(),
    });
    verdict
}

#[test]
fn running_stale_finished_respects_the_silence_floor_and_finished_gate() {
    let mut recorded_negatives = 0;
    for (path, fixture) in fixtures() {
        if fixture.evidence.phase != TurnPhase::Running {
            continue;
        }
        let Some(recorded) = fixture.recorded_verdict.as_ref() else {
            continue;
        };
        let verdict = recorded_turn_verdict(recorded);
        assert!(
            fixture.evidence.silent_for_s < STALE_FINISHED_SILENCE.as_secs(),
            "{}: recorded running sample is not below the stale-finished floor",
            path.display()
        );
        assert!(
            !stale_finished(&fixture.evidence, &verdict),
            "{}: recorded running-phase verdict must not end before the silence floor",
            path.display()
        );
        if fixture.category == "silent-but-working" {
            recorded_negatives += 1;
        }
    }
    assert!(
        recorded_negatives >= 3,
        "fewer than three recorded running negatives"
    );

    // SO-X04 recorded work=finished at 0.92 and resumed in the same turn
    // around 310 s later. Its modern assessment would otherwise infer idle,
    // so this pins the 40-minute floor against the recorded verdict itself.
    let (_, x04) = fixtures()
        .into_iter()
        .find(|(_, fixture)| fixture.id == "O07")
        .expect("SO-X04 fixture is present");
    let recorded = x04
        .recorded_verdict
        .as_ref()
        .expect("SO-X04 has its recorded verdict");
    let recorded = recorded_turn_verdict(recorded);
    assert_eq!(recorded.work_state, WorkState::Finished);
    assert!((recorded.work_state_confidence - 0.92).abs() < 0.001);
    assert_eq!(
        recorded.assessment.as_ref().unwrap().action(false),
        Action::Finished
    );
    let mut x04_evidence = x04.evidence.clone();
    x04_evidence.silent_for_s = 310;
    assert!(!stale_finished(&x04_evidence, &recorded));

    // O01 is the incident: it is short-silent in the captured evidence, but a
    // threshold-qualified modern verdict becomes stale-finished at 40 minutes.
    let (_, incident) = fixtures()
        .into_iter()
        .find(|(_, fixture)| fixture.id == "O01")
        .expect("incident fixture is present");
    let mut incident_evidence = incident.evidence.clone();
    incident_evidence.silent_for_s = STALE_FINISHED_SILENCE.as_secs();
    assert!(!stale_finished(&incident.evidence, &finished_verdict(0.80)));
    assert!(stale_finished(&incident_evidence, &finished_verdict(0.80)));

    let closing_reply = closing_reply_verdict(STALE_REPLY_PROBABILITY);
    assert_ne!(
        closing_reply.assessment.as_ref().unwrap().action(false),
        Action::Finished,
        "the reply path must qualify independently of work=finished"
    );
    assert!(!stale_finished(&incident.evidence, &closing_reply));
    assert!(stale_finished(&incident_evidence, &closing_reply));
    assert!(!stale_finished(
        &incident_evidence,
        &closing_reply_verdict(STALE_REPLY_PROBABILITY - 0.01)
    ));
}
