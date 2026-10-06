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

use mj_core::activity::verdict::TurnEvidence;
use mj_core::activity::{ActivityFacts, InFlightToolCall, quiet_at};
use mj_core::assessment::{Action, Verdict};
use mj_core::relay::RelayExecutionState;
use serde::Deserialize;

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
