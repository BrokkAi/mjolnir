//! The worker holds its own journal writes while a checkpoint barrier holds
//! its cut (campaign finding I2-3).
use super::test_support::*;
use super::*;
use agent_client_protocol::schema::v1::ContentChunk;

fn notification(text: &str) -> SessionUpdate {
    serde_json::from_value(serde_json::json!({
        "sessionUpdate": "session_info_update",
        "title": text,
    }))
    .unwrap()
}

fn agent_output(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text)))
}

fn journal(relay: &DurableRelay) -> Vec<RelayEvent> {
    relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap()
}

fn notice_ordinal(relay: &DurableRelay, text: &str) -> Option<u64> {
    journal(relay)
        .into_iter()
        .find_map(|event| match event.observation {
            RelayObservation::Notice { message } if message == text => Some(event.ordinal),
            _ => None,
        })
}

fn title_recorded(relay: &DurableRelay, text: &str) -> bool {
    journal(relay).iter().any(|event| {
        serde_json::to_string(&event.observation)
            .unwrap()
            .contains(text)
    })
}

fn configured(temp: &tempfile::TempDir) -> DurableRelay {
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            native_continuity_lost: false,
            resumed: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    relay
}

/// I2-3: the harness spoke after the close's cut and the worker refused the
/// Close. Output that changes no work now waits, so the Close matches its cut,
/// and a sealed relay drops what waited instead of writing it past the seal.
#[test]
fn a_close_seals_its_cut_although_the_harness_spoke_after_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let cut = ready_checkpoint(&mut relay, "close-barrier");

    relay
        .record_session_update(notification("a title chosen after the turn"))
        .unwrap();
    relay
        .record_observation(RelayObservation::Notice {
            message: "said after the cut".into(),
        })
        .unwrap();
    assert!(relay.worker_writes_held());
    assert_eq!(relay.operational_state().latest_ordinal, cut.ordinal);

    submit_relay(
        &mut relay,
        "close-command",
        RelayCommand::Close {
            barrier_command_id: "close-barrier".into(),
            expected: cut,
        },
    );
    submit_relay(
        &mut relay,
        "close-checkpoint-complete",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "close-barrier".into(),
        },
    );
    assert!(!relay.worker_writes_held());
    assert!(!title_recorded(&relay, "a title chosen after the turn"));
    assert_eq!(notice_ordinal(&relay, "said after the cut"), None);
}

/// A routine checkpoint releases its barrier after capture and the session
/// goes on. What waited is journaled then, in the order it arrived.
#[test]
fn a_released_barrier_journals_what_waited_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let cut = ready_checkpoint(&mut relay, "routine-barrier");
    for text in ["first", "second", "third"] {
        relay
            .record_observation(RelayObservation::Notice {
                message: text.into(),
            })
            .unwrap();
    }
    assert_eq!(relay.operational_state().latest_ordinal, cut.ordinal);

    let released = submit_relay(
        &mut relay,
        "routine-release",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "routine-barrier".into(),
        },
    );
    let first = notice_ordinal(&relay, "first").unwrap();
    let second = notice_ordinal(&relay, "second").unwrap();
    let third = notice_ordinal(&relay, "third").unwrap();
    assert!(released < first && first < second && second < third);
}

/// A barrier whose controller disconnects ends the hold the same way.
#[test]
fn a_cancelled_barrier_journals_what_waited() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    ready_checkpoint(&mut relay, "abandoned-barrier");
    relay
        .record_observation(RelayObservation::Notice {
            message: "waited".into(),
        })
        .unwrap();
    assert_eq!(notice_ordinal(&relay, "waited"), None);
    relay
        .cancel_checkpoint_barrier_on_disconnect("abandoned-barrier")
        .unwrap();
    assert!(notice_ordinal(&relay, "waited").is_some());
}

/// A turn the harness starts on its own during a routine barrier means the
/// agent may be writing to the workspace while it is captured. The daemon
/// abandons that archive when it sees the turn start past its cursor, so the
/// turn must reach the journal at once, as it did before writes were held:
/// the hold ends, what waited goes first, and then the turn.
#[test]
fn a_harness_turn_during_a_routine_barrier_ends_the_hold() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    relay.set_harness_turn_policy(HarnessTurnPolicy::ClaudeAdapter);
    let cut = ready_checkpoint(&mut relay, "routine-barrier");
    relay
        .record_observation(RelayObservation::Notice {
            message: "before the turn".into(),
        })
        .unwrap();
    assert_eq!(relay.operational_state().latest_ordinal, cut.ordinal);

    relay
        .record_session_update(agent_output("working on my own"))
        .unwrap();

    assert!(!relay.worker_writes_held());
    let state = relay.operational_state();
    let turn_started = state
        .last_harness_turn_started_ordinal
        .expect("the harness turn is journaled");
    assert!(turn_started > cut.ordinal);
    assert!(notice_ordinal(&relay, "before the turn").unwrap() < turn_started);
    // Nothing waits for this barrier any more, so the turn's output is
    // journaled as it arrives.
    relay
        .record_observation(RelayObservation::Notice {
            message: "during the turn".into(),
        })
        .unwrap();
    assert!(notice_ordinal(&relay, "during the turn").unwrap() > turn_started);
}

/// A question for a person cannot wait behind a barrier: nobody could see it
/// to answer it. It ends the hold and is journaled at once.
#[test]
fn a_question_during_a_barrier_is_journaled_at_once() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let cut = ready_checkpoint(&mut relay, "question-barrier");
    relay
        .record_observation(RelayObservation::Notice {
            message: "before the question".into(),
        })
        .unwrap();
    let asked = relay
        .record_observation(RelayObservation::ElicitationRequested {
            request: mj_core::elicitation::ElicitationRequest {
                id: "question-1".into(),
                message: "May I continue?".into(),
                title: None,
                description: None,
                fields: Vec::new(),
            },
        })
        .unwrap();
    assert!(asked > cut.ordinal);
    assert!(notice_ordinal(&relay, "before the question").unwrap() < asked);
    assert!(!relay.worker_writes_held());
}
