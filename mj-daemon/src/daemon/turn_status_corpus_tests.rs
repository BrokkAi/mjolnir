use std::collections::BTreeMap;

use mj_client::daemon::{
    RuntimeLifecycleKind, RuntimeLifecycleView, RuntimeMaterializedSession, RuntimeSessionView,
};
use mj_client::review::{ReviewerQuestion, RuntimeReviewView};
use mj_client::runtime_feed::{RuntimeProjection, SessionTail};
use mj_controller::database::{CommittedState, CommittedTurn};
use mj_core::activity::ActivityState;
use mj_core::config::{HarnessKind, HarnessProfile};
use mj_core::elicitation::ElicitationRequest;
use mj_core::native_agent::{NativeAgent, NativeAgentState, NativeAgentSummary};
use mj_core::relay::{RelayOperationalState, RelaySnapshot};
use mj_core::review::driver::TurnReviewPhase;
use mj_core::state::{
    MaterializedExecutionState, MaterializedSession, MaterializedTurn, MaterializedTurnOutcome,
    ProjectionWindow, SessionState, State,
};
use mj_core::subagent::{SubagentHandback, SubagentKind, SubagentReport};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    description: String,
    sessions: Vec<Session>,
    subagents: Vec<Relation>,
    native_agents: Vec<Native>,
    reviews: Vec<Review>,
    committed_subagent_holds: Vec<Hold>,
    expected: BTreeMap<String, Expected>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    id: String,
    state: SessionState,
    connected: bool,
    #[serde(default)]
    archived: bool,
    lifecycle: Option<RuntimeLifecycleKind>,
    harness_kind: HarnessKind,
    operational: Option<Map<String, Value>>,
    #[serde(default)]
    transcript: Transcript,
    last_error: Option<String>,
    #[serde(default)]
    viewed_through_event_ordinal: u64,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Transcript {
    execution: MaterializedExecutionState,
    active_turn: Option<MaterializedTurn>,
    pending_elicitations: Vec<ElicitationRequest>,
    unread: Option<u64>,
    last_turn_outcome: Option<MaterializedTurnOutcome>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relation {
    child: String,
    parent: String,
    kind: SubagentKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Native {
    owner: String,
    parent_session_id: Option<String>,
    session_id: String,
    stable_id: Option<String>,
    state: NativeAgentState,
    pending_input: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    session_id: String,
    phase: TurnReviewPhase,
    has_questions: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hold {
    child: String,
    handback_recorded: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    attention: mj_core::attention::SessionAttention,
    working: bool,
    idle: bool,
    working_children: usize,
    working_descendants: usize,
    unfinished_children: usize,
    subtree_idle: bool,
    working_subagents: usize,
    activity_kind: String,
}

fn build(case: &Case) -> (RuntimeProjection, CommittedState) {
    let mut runtime = RuntimeProjection::default();
    let mut config = super::tests::runtime_test_config();
    let mut turns = mj_core::snapshot_map::SnapshotMap::default();
    let mut subagent_reports = mj_core::snapshot_map::SnapshotMap::default();
    for session in &case.sessions {
        let id = &session.id;
        let mut record = super::tests::runtime_test_session(id, "workspace", session.state);
        record.harness_kind = session.harness_kind;
        record.last_profile = serde_json::to_value(session.harness_kind)
            .unwrap()
            .as_str()
            .unwrap()
            .into();
        config
            .profiles
            .entry(record.last_profile.clone())
            .or_insert_with(|| HarnessProfile::new(session.harness_kind, "fixture-profile"));
        record.archived = session.archived;
        record.last_error.clone_from(&session.last_error);
        record.viewed_through_event_ordinal = session.viewed_through_event_ordinal;
        assert!(runtime.records.insert(id.clone(), record).is_none());
        if let Some(kind) = session.lifecycle {
            runtime.metadata.lifecycles.push(RuntimeLifecycleView {
                operation_id: format!("operation-{id}"),
                move_operation_id: None,
                cancellable: false,
                session_id: id.clone(),
                kind,
                started_at_epoch_seconds: 0,
                active_stages: Vec::new(),
                resume_destination: None,
                notice: None,
            });
        }

        let operational = session.operational.as_ref().map(|fields| {
            let defaults = RelaySnapshot::new(id.clone()).operational_state();
            let Value::Object(mut body) = json!(defaults) else {
                unreachable!()
            };
            body.extend(fields.clone());
            let live: RelayOperationalState = serde_json::from_value(Value::Object(body))
                .unwrap_or_else(|error| panic!("{}/{id} operational: {error}", case.name));
            let published = json!(live);
            for (key, value) in fields {
                assert!(
                    published.get(key).is_some() || value.is_null(),
                    "{}/{id}: unused operational field {key}",
                    case.name
                );
            }
            live.into()
        });
        let transcript = &session.transcript;
        let mut materialized = MaterializedSession::empty(id);
        materialized.execution = transcript.execution;
        materialized
            .pending_elicitations
            .clone_from(&transcript.pending_elicitations);
        materialized.active_turn.clone_from(&transcript.active_turn);
        materialized
            .last_turn_outcome
            .clone_from(&transcript.last_turn_outcome);
        runtime.transcripts.insert(
            id.clone(),
            SessionTail::of(
                &materialized,
                &ProjectionWindow {
                    latest_unread_event_ordinal: transcript.unread,
                    ..Default::default()
                },
                0,
            ),
        );
        turns.insert(
            id.clone(),
            CommittedTurn {
                state: (
                    materialized.execution,
                    materialized.active_turn,
                    materialized.last_turn_outcome,
                ),
                queued_command_ids: vec![],
                failed_message: None,
                pending_form_times: BTreeMap::new(),
            },
        );
        runtime.sessions.insert(
            id.clone(),
            RuntimeSessionView {
                materialized: Some(
                    RuntimeMaterializedSession {
                        execution: materialized.execution,
                        pending_elicitations: materialized.pending_elicitations,
                        ..Default::default()
                    }
                    .into(),
                ),
                materialized_only: operational.is_none(),
                attention: Default::default(),
                policy_config_keys: Vec::new(),
                session_id: id.clone(),
                projection_ordinal: 0,
                projection_digest: String::new(),
                operational,
                latest_credential_sync_signal: None,
                connected: session.connected,
                error: None,
            },
        );
    }
    for relation in &case.subagents {
        assert!(runtime.records.contains_key(&relation.parent));
        assert!(runtime.records.contains_key(&relation.child));
        let mut record = super::tests::runtime_test_subagent(&relation.child, &relation.parent);
        record.kind = relation.kind;
        record.handback_tool = case
            .committed_subagent_holds
            .iter()
            .any(|hold| hold.child == relation.child);
        runtime
            .subagents
            .insert(relation.child.clone(), record.into());
    }
    for hold in &case.committed_subagent_holds {
        assert!(runtime.subagents.contains_key(&hold.child));
        let outcome = turns[&hold.child].state.2.as_ref().unwrap();
        subagent_reports.insert(
            hold.child.clone(),
            SubagentReport {
                handback: hold.handback_recorded.then(|| SubagentHandback {
                    command_id: outcome.command_id.clone(),
                    message: "Work complete.".into(),
                    recorded_at_ms: outcome.completed_at_ms,
                }),
                awaited_ordinal: outcome.accepted_ordinal,
                ..Default::default()
            },
        );
    }
    for spec in &case.native_agents {
        assert!(runtime.records.contains_key(&spec.owner));
        let agent = NativeAgent {
            owner_session_id: spec.owner.clone(),
            session_id: spec.session_id.clone(),
            parent_session_id: spec.parent_session_id.clone(),
            name: "Native task".into(),
            task: "Inspect files".into(),
            capabilities: Default::default(),
            state: spec.state,
            availability: Default::default(),
            availability_reason: None,
            stable_id: spec.stable_id.clone(),
        };
        runtime.native_agents.insert(
            agent.view_id(),
            NativeAgentSummary {
                agent,
                generation_ordinal: 1,
                projection_ordinal: 0,
                projection_digest: String::new(),
                pending_input: spec.pending_input,
                pending_input_request_id: spec.pending_input.then(|| "question".into()),
                pending_input_message: spec.pending_input.then(|| "Proceed?".into()),
                pending_input_raised_at_ms: spec.pending_input.then_some(1),
            },
        );
    }
    for review in &case.reviews {
        let questions = if review.has_questions {
            vec![ReviewerQuestion {
                role: "reviewer".into(),
                request: serde_json::from_value(json!({
                    "id": "question", "message": "Proceed?", "fields": []
                }))
                .unwrap(),
            }]
        } else {
            vec![]
        };
        runtime.metadata.reviews.push(RuntimeReviewView {
            review_id: "review".into(),
            roots: vec![],
            session_id: review.session_id.clone(),
            child_session_id: None,
            phase: review.phase.clone(),
            roles: vec![],
            status: "Review status".into(),
            verdict: None,
            questions,
        });
    }
    runtime.metadata.config = config.into();
    let committed = CommittedState {
        sequence: 1,
        state: State {
            sessions: runtime.records.clone(),
            environment_owners: runtime
                .records
                .keys()
                .map(|id| (id.clone(), "parent".into()))
                .collect(),
            subagents: runtime.subagents.clone(),
            ..Default::default()
        },
        retired_container_allocations: Default::default(),
        moves: Default::default(),
        native_agents: Default::default(),
        startup_groups: Default::default(),
        subagent_reports,
        turns,
        wait_revisions: Default::default(),
    };
    (runtime, committed)
}

fn activity_kind(activity: &ActivityState) -> String {
    let wire = json!(activity);
    let kind = wire["state"].as_str().unwrap();
    if let ActivityState::Unknown { last_known, .. } = activity {
        format!("{kind}/{}", activity_kind(last_known))
    } else {
        kind.to_owned()
    }
}

#[test]
fn turn_status_corpus() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../../tests/fixtures/turn-status-corpus.json")).unwrap();
    assert!(!cases.is_empty());
    let case_count = cases.len();
    let mut rows = 0;
    let mut absent = 0;
    let mut disagreements = 0;
    for case in cases {
        let (mut runtime, committed) = build(&case);
        assert!(!case.description.is_empty());
        assert!(case.expected.keys().eq(runtime.records.keys()));
        let previous = RuntimeProjection::default();
        super::verdicts::project_activity(&mut runtime, &previous, Some(&committed), None);
        super::attention::update(&previous, &mut runtime, Some(&committed));
        for id in runtime.records.keys() {
            runtime.sessions.get_mut(id).unwrap().attention =
                runtime.metadata.session_attention[id];
        }
        super::verdicts::stamp_needs_person(&mut runtime, &previous);
        let viewer =
            crate::server_runtime::snapshot::turn_status_corpus_viewer(&runtime, &committed);
        rows += case.expected.len();
        for (id, want) in &case.expected {
            let row = viewer.sessions.get(id).unwrap();
            let api = crate::server::api::ApiSession::from(row);
            let verdict = runtime.verdicts[id];
            let api_wire = json!(api);
            let fields = [
                (
                    "attention",
                    Some(json!(runtime.metadata.session_attention[id])),
                    json!(want.attention),
                ),
                (
                    "viewer.attention",
                    Some(json!(row.attention)),
                    json!(want.attention),
                ),
                (
                    "api.attention",
                    Some(json!(api.attention)),
                    json!(want.attention),
                ),
                ("working", Some(json!(verdict.working)), json!(want.working)),
                ("idle", Some(json!(verdict.idle)), json!(want.idle)),
                (
                    "working_children",
                    Some(json!(verdict.working_children)),
                    json!(want.working_children),
                ),
                (
                    "unfinished_children",
                    Some(json!(verdict.unfinished_children)),
                    json!(want.unfinished_children),
                ),
                ("api.is_idle", Some(json!(api.is_idle)), json!(want.idle)),
                (
                    "activity_state",
                    Some(json!(activity_kind(api.activity_state.as_ref().unwrap()))),
                    json!(want.activity_kind),
                ),
                (
                    "working_descendants",
                    Some(json!(verdict.working_descendants)),
                    json!(want.working_descendants),
                ),
                (
                    "api.subtree_idle",
                    api_wire.get("subtree_idle").cloned(),
                    json!(want.subtree_idle),
                ),
                (
                    "api.working_subagents",
                    api_wire.get("working_subagents").cloned(),
                    json!(want.working_subagents),
                ),
            ];
            for (field, got, expected) in fields {
                let prefix = format!("{}/{id} {field}", case.name);
                match got {
                    Some(got) if got == expected => println!("PASS {prefix} = {got}"),
                    Some(got) => {
                        println!("DIFF {prefix} expected {expected}, got {got}");
                        disagreements += 1;
                    }
                    None => {
                        println!("ABSENT {prefix} expected {expected}");
                        absent += 1;
                    }
                }
            }
        }
    }
    println!("{case_count} cases, {rows} rows, {absent} absent, {disagreements} disagreements");
    assert_eq!(absent, 0, "turn-status corpus missing fields printed above");
    assert_eq!(
        disagreements, 0,
        "turn-status corpus disagreements printed above"
    );
}
