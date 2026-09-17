//! State-transition tests for the shared activity mechanism.
//!
//! Every test drives the functions directly with made-up facts: no relay, no
//! worker, no daemon. That is the point of the module — the decisions these
//! tests cover used to live in six places, none of which could be tested
//! without a live session.

use super::*;
use agent_client_protocol::schema::v1::ToolCallStatus;

const MINUTE: u64 = 60_000;
const NOW: i64 = 1_000_000_000;

fn tool(id: &str, started_at_ms: i64) -> InFlightToolCall {
    InFlightToolCall {
        tool_call_id: id.to_owned(),
        status: ToolCallStatus::InProgress,
        started_at_ms,
    }
}

fn policy(silence_minutes: u64, tool_call_minutes: u64) -> StallPolicy {
    StallPolicy {
        silence: (silence_minutes > 0).then(|| Duration::from_millis(silence_minutes * MINUTE)),
        tool_call: (tool_call_minutes > 0)
            .then(|| Duration::from_millis(tool_call_minutes * MINUTE)),
    }
}

/// The failure in issue #1020: a harness blocked in a long tool call sends no
/// protocol traffic for the whole of a twenty-minute build, and used to have
/// its turn failed for it.
#[test]
fn a_running_tool_call_is_not_a_stall() {
    let facts = ActivityFacts {
        execution: RelayExecutionState::Running,
        prompt_started_at_ms: Some(NOW - 40 * MINUTE as i64),
        last_acp_activity_at_ms: Some(NOW - 30 * MINUTE as i64),
        tools_in_flight: vec![tool("cargo-nextest", NOW - 30 * MINUTE as i64)],
        ..ActivityFacts::default()
    };
    assert_eq!(
        stall_verdict(&facts, policy(10, 240), NOW),
        StallVerdict::Live
    );
}

#[test]
fn silence_with_nothing_in_flight_is_a_stall() {
    let facts = ActivityFacts {
        execution: RelayExecutionState::Running,
        last_acp_activity_at_ms: Some(NOW - 30 * MINUTE as i64),
        ..ActivityFacts::default()
    };
    assert_eq!(
        stall_verdict(&facts, policy(10, 240), NOW),
        StallVerdict::Silent {
            silent_ms: 30 * MINUTE
        }
    );
}

#[test]
fn a_tool_call_that_outlives_its_bound_is_a_stall_naming_the_call() {
    let facts = ActivityFacts {
        execution: RelayExecutionState::Running,
        last_acp_activity_at_ms: Some(NOW - 250 * MINUTE as i64),
        tools_in_flight: vec![tool("job-output", NOW - 250 * MINUTE as i64)],
        ..ActivityFacts::default()
    };
    assert_eq!(
        stall_verdict(&facts, policy(10, 240), NOW),
        StallVerdict::ToolCall {
            tool_call_id: "job-output".into(),
            running_ms: 250 * MINUTE,
            silent_ms: 250 * MINUTE,
        }
    );
}

#[test]
fn a_disabled_bound_never_trips_and_never_spins() {
    let running_a_day = ActivityFacts {
        last_acp_activity_at_ms: Some(NOW - 1_440 * MINUTE as i64),
        tools_in_flight: vec![tool("forever", NOW - 1_440 * MINUTE as i64)],
        ..ActivityFacts::default()
    };
    assert_eq!(
        stall_verdict(&running_a_day, policy(10, 0), NOW),
        StallVerdict::Live
    );
    assert!(policy(10, 0).next_check(&running_a_day, NOW) >= Duration::from_millis(250));

    let silent = ActivityFacts {
        last_acp_activity_at_ms: Some(NOW - 1_440 * MINUTE as i64),
        ..ActivityFacts::default()
    };
    assert_eq!(stall_verdict(&silent, policy(0, 240), NOW), StallVerdict::Live);
    assert!(!policy(0, 0).enabled());
}

/// A turn with no recorded activity at all has not started; absence of
/// evidence must not be read as silence.
#[test]
fn an_empty_activity_clock_is_not_a_stall() {
    let facts = ActivityFacts::default();
    assert_eq!(stall_verdict(&facts, policy(10, 240), NOW), StallVerdict::Live);
}

#[test]
fn the_next_check_waits_for_the_bound_that_can_trip_first() {
    let with_tool = ActivityFacts {
        last_acp_activity_at_ms: Some(NOW - 9 * MINUTE as i64),
        tools_in_flight: vec![tool("build", NOW - 100 * MINUTE as i64)],
        ..ActivityFacts::default()
    };
    // The silence bound does not apply while a tool runs, so the wait is the
    // remainder of the tool bound, not the remainder of the silence bound.
    assert_eq!(
        policy(10, 240).next_check(&with_tool, NOW),
        Duration::from_millis(140 * MINUTE)
    );

    let silent = ActivityFacts {
        last_acp_activity_at_ms: Some(NOW - 9 * MINUTE as i64),
        ..ActivityFacts::default()
    };
    assert_eq!(
        policy(10, 240).next_check(&silent, NOW),
        Duration::from_millis(MINUTE)
    );
}

/// The failure in issue #1025: while the daemon cannot see the worker it used
/// to report the default value of an enum, which is `Idle`, about a session
/// whose turn was still running.
#[test]
fn a_disconnected_daemon_never_reports_idle() {
    let state = while_disconnected(
        MaterializedExecutionState::Running {
            started_at_ms: NOW - 70 * MINUTE as i64,
        },
        Some(NOW),
    );
    assert!(!state.is_idle(), "{state:?}");
    assert!(state.has_work_in_flight(), "{state:?}");
    assert!(state.is_working(), "{state:?}");
    assert_eq!(state.chat_phase(), RelayExecutionState::Running);
    assert_eq!(
        state.last_known(),
        &ActivityState::Turn {
            started_at_ms: Some(NOW - 70 * MINUTE as i64)
        }
    );

    let was_idle = while_disconnected(MaterializedExecutionState::Idle, Some(NOW));
    assert!(!was_idle.is_idle(), "unknown is never confirmed idle");
    assert_eq!(was_idle.chat_phase(), RelayExecutionState::Idle);
}

/// A worker that is truly gone must still be recoverable: reporting `Unknown`
/// about it would make the restart and checkpoint paths wait forever for work
/// that no longer exists.
#[test]
fn a_lost_worker_is_still_recovered() {
    let closed = while_disconnected(MaterializedExecutionState::Closed, Some(NOW));
    assert_eq!(closed, ActivityState::Closed);
    assert!(!closed.has_work_in_flight(), "a closed session holds nothing");
    assert!(!closed.is_working());

    // The same through the facts the controller syncs from a worker that has
    // stopped: nothing blocks the restart path.
    let facts = ActivityFacts {
        execution: RelayExecutionState::Closed,
        // Stale live evidence from before it stopped must not resurrect it.
        tools_in_flight: vec![tool("orphan", NOW - MINUTE as i64)],
        queued_commands: 2,
        active_agent_terminals: 1,
        ..ActivityFacts::default()
    };
    assert_eq!(classify(&facts), ActivityState::Closed);
    assert!(!has_work_in_flight(&facts));
    assert_eq!(checkpoint_blocker(&facts, HarnessKind::Kimi), None);
}

#[test]
fn every_way_of_being_busy_is_work_in_flight() {
    let quiet = ActivityFacts {
        goal_synchronized: true,
        background_work_known: Some(true),
        ..ActivityFacts::default()
    };
    assert!(is_quiet(&quiet));
    assert!(classify(&quiet).is_idle());
    assert!(safe_to_replace(&quiet, HarnessKind::Codex));
    assert!(safe_to_replace(&quiet, HarnessKind::Kimi));

    // Rows where the session is doing something: it holds work *and* its
    // state says so.
    type MakeBusy = fn(&mut ActivityFacts);
    let doing: Vec<(&str, MakeBusy)> = vec![
        ("a prompt is in flight", |facts| {
            facts.prompt_started_at_ms = Some(NOW)
        }),
        ("the harness started a turn", |facts| {
            facts.harness_turn_started_at_ms = Some(NOW)
        }),
        ("a close is in progress", |facts| {
            facts.execution = RelayExecutionState::Closing
        }),
        ("a tool call is open", |facts| {
            facts.tools_in_flight = vec![tool("bash", NOW)]
        }),
        ("a background command is running", |facts| {
            facts.background_commands = 1
        }),
        ("a user shell is open", |facts| facts.active_user_shells = 1),
        ("a goal is active", |facts| facts.goal_active = true),
        ("a goal is running", |facts| facts.goal_running = true),
    ];
    // Rows where the session holds work without doing anything right now. Its
    // state is idle and it is still not safe to kill the worker: these are
    // separate questions and this is where they legitimately differ.
    let holding: Vec<(&str, MakeBusy)> = vec![
        // A bare running flag is the projection lagging behind a turn that
        // has ended. It is not enough to claim the agent is working, and far
        // too much to ignore when deciding whether to kill the worker.
        ("the execution flag says running", |facts| {
            facts.execution = RelayExecutionState::Running
        }),
        ("a command is queued", |facts| facts.queued_commands = 1),
        ("an agent terminal is open", |facts| {
            facts.active_agent_terminals = 1
        }),
        ("a goal resume is pending", |facts| {
            facts.goal_pending_resume = true
        }),
        ("a goal decision is waiting", |facts| {
            facts.goal_decision = true
        }),
        ("the ACP session is not open yet", |facts| {
            facts.acp_ready = Some(false)
        }),
        ("provider background work is unknown", |facts| {
            facts.background_work_known = Some(false)
        }),
    ];
    for (reason, make_busy) in doing.iter().chain(holding.iter()) {
        let mut facts = quiet.clone();
        make_busy(&mut facts);
        assert!(has_work_in_flight(&facts), "{reason}");
        assert!(!is_quiet(&facts), "{reason}");
        assert!(!safe_to_replace(&facts, HarnessKind::Claude), "{reason}");
    }
    for (reason, make_busy) in &doing {
        let mut facts = quiet.clone();
        make_busy(&mut facts);
        assert!(!classify(&facts).is_idle(), "{reason}");
    }
    for (reason, make_busy) in &holding {
        let mut facts = quiet.clone();
        make_busy(&mut facts);
        assert!(
            classify(&facts).is_idle(),
            "nothing is running, so the session reads idle even though it still holds work: {reason}"
        );
    }

    // A held barrier is the one reason `is_quiet` refuses that is not itself
    // work: a caller already holding one asks whether anything else is.
    let mut latched = quiet.clone();
    latched.checkpoint_barrier = true;
    assert!(!is_quiet(&latched));
    assert!(!has_work_in_flight(&latched));
}

/// A running flag needs something live to corroborate it, because the durable
/// projection can lag behind a turn that has already ended. With corroboration
/// the session is working; without it the flag alone proves nothing.
#[test]
fn a_running_flag_alone_is_not_a_running_turn() {
    let flag_only = ActivityFacts {
        execution: RelayExecutionState::Running,
        ..ActivityFacts::default()
    };
    assert!(classify(&flag_only).is_idle());
    assert!(has_work_in_flight(&flag_only), "but it is still not safe to kill");
    // The phase a session reports keeps naming the flag, so nothing that read
    // `chat_phase` before reads something weaker now.
    assert_eq!(chat_phase(&flag_only), RelayExecutionState::Running);

    for corroboration in [
        |facts: &mut ActivityFacts| facts.current_step_started_at_ms = Some(NOW),
        |facts: &mut ActivityFacts| facts.goal_running = true,
        |facts: &mut ActivityFacts| facts.prompt_started_at_ms = Some(NOW),
    ] {
        let mut facts = flag_only.clone();
        corroboration(&mut facts);
        assert!(classify(&facts).is_working(), "{facts:?}");
        assert_eq!(chat_phase(&facts), RelayExecutionState::Running);
    }

    // And a live turn the flag has not caught up with still reports running.
    let lagging = ActivityFacts {
        execution: RelayExecutionState::Idle,
        harness_turn_started_at_ms: Some(NOW),
        ..ActivityFacts::default()
    };
    assert_eq!(chat_phase(&lagging), RelayExecutionState::Running);
}

#[test]
fn what_the_session_is_doing_is_reported_in_order_of_precedence() {
    let idle = ActivityFacts {
        idle_since_ms: Some(NOW),
        ..ActivityFacts::default()
    };
    assert_eq!(classify(&idle), ActivityState::Idle { since_ms: Some(NOW) });

    let goal = ActivityFacts {
        goal_active: true,
        ..ActivityFacts::default()
    };
    assert_eq!(classify(&goal), ActivityState::Goal);

    let background = ActivityFacts {
        goal_active: true,
        background_commands: 1,
        background_started_at_ms: Some(NOW),
        ..ActivityFacts::default()
    };
    assert_eq!(
        classify(&background),
        ActivityState::Background {
            started_at_ms: Some(NOW)
        }
    );

    // A tool call outranks background work, and is visible even when the
    // durable execution flag has not caught up with it.
    let tool_only = ActivityFacts {
        background_commands: 1,
        tools_in_flight: vec![tool("bash", NOW - MINUTE as i64)],
        ..ActivityFacts::default()
    };
    assert_eq!(
        classify(&tool_only),
        ActivityState::Tool {
            tool_call_id: "bash".into(),
            started_at_ms: NOW - MINUTE as i64,
        }
    );
    assert_eq!(classify(&tool_only).chat_phase(), RelayExecutionState::Running);

    // A turn marker outranks everything below it.
    let turn = ActivityFacts {
        harness_turn_started_at_ms: Some(NOW),
        tools_in_flight: vec![tool("bash", NOW)],
        background_commands: 1,
        ..ActivityFacts::default()
    };
    assert_eq!(
        classify(&turn),
        ActivityState::Turn {
            started_at_ms: Some(NOW)
        }
    );

    // Lifecycle outranks everything, including live work, because a closed
    // worker owns nothing whatever its last snapshot said.
    for (execution, expected) in [
        (RelayExecutionState::Closing, ActivityState::Closing),
        (RelayExecutionState::Closed, ActivityState::Closed),
    ] {
        let facts = ActivityFacts {
            execution,
            harness_turn_started_at_ms: Some(NOW),
            ..ActivityFacts::default()
        };
        assert_eq!(classify(&facts), expected);
    }
}

/// A newer worker may publish a state this build does not know. Failing to
/// deserialize it would drop the whole snapshot and take every other fact in
/// it with it, so an unknown state lands on a cautious value instead.
#[test]
fn an_unrecognized_published_state_is_cautious_rather_than_fatal() {
    let from_the_future =
        serde_json::json!({"state": "compacting", "started_at_ms": 12_345_i64});
    let state: ActivityState =
        serde_json::from_value(from_the_future).expect("an unknown state must still deserialize");
    assert_eq!(state, ActivityState::Unrecognized);
    assert!(!state.is_idle());
    assert!(state.has_work_in_flight());
    assert_eq!(state.chat_phase(), RelayExecutionState::Running);

    // And a state this build does know still round-trips unchanged.
    let known = ActivityState::Tool {
        tool_call_id: "bash".into(),
        started_at_ms: 7,
    };
    let text = serde_json::to_string(&known).expect("serialize");
    assert_eq!(
        serde_json::from_str::<ActivityState>(&text).expect("round trip"),
        known
    );
}

#[test]
fn checkpoint_admission_asks_only_about_provider_owned_work() {
    // A turn is not a reason to refuse a checkpoint barrier; it is a reason to
    // try again later, which is a different decision with a different caller.
    let working = ActivityFacts {
        execution: RelayExecutionState::Running,
        goal_synchronized: true,
        background_work_known: Some(true),
        ..ActivityFacts::default()
    };
    assert_eq!(checkpoint_blocker(&working, HarnessKind::Claude), None);
    assert!(has_work_in_flight(&working));

    let unsynchronized_codex = ActivityFacts {
        goal_synchronized: false,
        ..ActivityFacts::default()
    };
    assert!(checkpoint_blocker(&unsynchronized_codex, HarnessKind::Codex).is_some());
    assert_eq!(
        checkpoint_blocker(&unsynchronized_codex, HarnessKind::Claude),
        None
    );

    let unknown_kimi = ActivityFacts {
        background_work_known: None,
        ..ActivityFacts::default()
    };
    assert!(checkpoint_blocker(&unknown_kimi, HarnessKind::Kimi).is_some());

    let busy_kimi = ActivityFacts {
        background_work_known: Some(true),
        background_commands: 1,
        ..ActivityFacts::default()
    };
    assert!(checkpoint_blocker(&busy_kimi, HarnessKind::Kimi).is_some());
}
