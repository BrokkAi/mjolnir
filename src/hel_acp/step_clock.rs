//! When the step the agent is on right now began.
//!
//! ACP has no notion of a step. An agent streams one message as hundreds of
//! `agent_message_chunk` updates and revises one tool call a dozen times, so
//! the time since the last ACP message is almost always a fraction of a
//! second and says nothing about progress. What a person reads as a step is
//! coarser: this thought, this message, this tool call, this run of a
//! command.
//!
//! So this reduces each update to a [`StepSignature`] and restarts the clock
//! only when the signature changes. Chunks of the same message, appended tool
//! output under an unchanged status, and the metadata updates every harness
//! sprays between them all leave the clock alone.
//!
//! The rule comes from recordings of all five bridges under
//! `testdata/step_clock`; the tests below replay them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};

use crate::clock::epoch_millis;

/// What the agent is doing, at the granularity a person reads as one step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StepSignature {
    /// Streaming prose to the user.
    Message,
    /// Streaming its reasoning.
    Thought,
    /// Publishing or revising its plan.
    Plan,
    /// Working on one tool call. The status is part of the signature because
    /// `pending`, `in_progress` and `completed` are separate steps, while
    /// content appended under an unchanged status is the same step going on.
    Tool { id: String, status: ToolCallStatus },
}

#[derive(Debug, Default)]
struct StepState {
    started_at_ms: Option<i64>,
    signature: Option<StepSignature>,
    /// The status each tool call last reported. A `tool_call_update` may omit
    /// `status` and only add content or fix a title, and every bridge does;
    /// such an update belongs to whatever the call was already doing.
    tool_statuses: BTreeMap<String, ToolCallStatus>,
}

/// Process-local clock for the current step, shared between the ACP client
/// handlers and the relay that reports operational state.
///
/// Like [`crate::hel_worker::AcpActivityClock`] it stays out of the durable
/// journal: this is render timing, not recoverable session history.
#[derive(Debug, Clone, Default)]
pub struct StepClock(Arc<Mutex<StepState>>);

impl StepClock {
    /// Epoch milliseconds the current step began, while one is in flight.
    pub fn started_at_ms(&self) -> Option<i64> {
        self.state().started_at_ms
    }

    /// A turn is starting: the first step begins now, with no history behind
    /// it from the turn before.
    pub fn begin_turn(&self) {
        self.begin_turn_at(epoch_millis());
    }

    /// The turn is over, so no step is in flight. Whatever the next turn does
    /// first opens a step of its own.
    pub fn end_turn(&self) {
        let mut state = self.state();
        state.started_at_ms = None;
        state.signature = None;
        state.tool_statuses.clear();
    }

    /// The agent asked the client to do something: run a terminal, answer a
    /// permission prompt, serve an extension request. That is work starting
    /// even when the update stream has not moved on, so the step restarts —
    /// but the signature stays, so the updates that follow the same tool call
    /// under the same status still do not restart it again.
    pub fn begin_client_work(&self) {
        self.begin_client_work_at(epoch_millis());
    }

    /// Fold one session update in, restarting the step when it opens one.
    pub fn observe(&self, update: &SessionUpdate) {
        self.observe_at(update, epoch_millis());
    }

    fn state(&self) -> std::sync::MutexGuard<'_, StepState> {
        self.0.lock().expect("ACP step clock lock poisoned")
    }

    fn begin_turn_at(&self, now_ms: i64) {
        let mut state = self.state();
        state.started_at_ms = Some(now_ms);
        state.signature = None;
        state.tool_statuses.clear();
    }

    fn begin_client_work_at(&self, now_ms: i64) {
        self.state().started_at_ms = Some(now_ms);
    }

    fn observe_at(&self, update: &SessionUpdate, now_ms: i64) {
        let mut state = self.state();
        let Some(signature) = signature_of(update, &mut state.tool_statuses) else {
            return;
        };
        if state.signature.as_ref() == Some(&signature) {
            return;
        }
        state.signature = Some(signature);
        state.started_at_ms = Some(now_ms);
    }
}

/// The step an update belongs to, or `None` when it says nothing about what
/// the agent is doing.
///
/// Usage, session-info, mode, config and available-command updates are the
/// `None` cases. Every bridge interleaves them with real work — Claude Code
/// sends `usage_update` between two updates of the same running tool call —
/// so treating them as steps would restart the clock at random.
fn signature_of(
    update: &SessionUpdate,
    tool_statuses: &mut BTreeMap<String, ToolCallStatus>,
) -> Option<StepSignature> {
    match update {
        SessionUpdate::AgentMessageChunk(_) => Some(StepSignature::Message),
        SessionUpdate::AgentThoughtChunk(_) => Some(StepSignature::Thought),
        SessionUpdate::Plan(_) => Some(StepSignature::Plan),
        SessionUpdate::ToolCall(call) => {
            let id = call.tool_call_id.to_string();
            tool_statuses.insert(id.clone(), call.status);
            Some(StepSignature::Tool {
                id,
                status: call.status,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.to_string();
            let status = match update.fields.status {
                Some(status) => {
                    tool_statuses.insert(id.clone(), status);
                    status
                }
                // No status of its own: the call is still doing what it was.
                // A call whose creation this connection never saw defaults to
                // `pending`, which is what ACP says an unstated status means.
                None => tool_statuses.get(&id).copied().unwrap_or_default(),
            };
            Some(StepSignature::Tool { id, status })
        }
        // The user's own message is not the agent working.
        SessionUpdate::UserMessageChunk(_) => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line of a recorded bridge session: either an update the agent
    /// sent, or a request it made of the client.
    #[derive(serde::Deserialize)]
    struct Recorded {
        at_ms: i64,
        #[serde(default)]
        update: Option<SessionUpdate>,
        #[serde(default)]
        client_request: Option<String>,
    }

    /// Replay a recording and report the offset, in milliseconds from the
    /// prompt, at which each step began. A repeated offset never appears: the
    /// list is exactly the moments the clock restarted.
    fn step_starts(recording: &str) -> Vec<i64> {
        let clock = StepClock::default();
        clock.begin_turn_at(0);
        let mut starts = vec![0];
        for line in recording.lines().filter(|line| !line.trim().is_empty()) {
            let recorded: Recorded =
                serde_json::from_str(line).expect("recorded ACP line should parse");
            match (&recorded.update, &recorded.client_request) {
                (Some(update), _) => clock.observe_at(update, recorded.at_ms),
                (None, Some(_)) => clock.begin_client_work_at(recorded.at_ms),
                (None, None) => panic!("recorded line has neither an update nor a request"),
            }
            let started = clock.started_at_ms().expect("a turn is open");
            if starts.last() != Some(&started) {
                starts.push(started);
            }
        }
        starts
    }

    const CODEX: &str = include_str!("testdata/step_clock/codex.jsonl");
    const CLAUDE: &str = include_str!("testdata/step_clock/claude.jsonl");
    const KIMI: &str = include_str!("testdata/step_clock/kimi.jsonl");
    const GROK: &str = include_str!("testdata/step_clock/grok.jsonl");
    const DEEPSEEK: &str = include_str!("testdata/step_clock/deepseek.jsonl");

    #[test]
    fn codex_streams_a_message_and_a_tool_call_as_four_steps() {
        // Preamble message, the `List files` call running, the same call
        // completing, the thought, then the answer. The `usage_update` and
        // `session_info_update` either side of the answer are not steps, so
        // the last step stays open at 10186 rather than restarting at 13800.
        assert_eq!(
            step_starts(CODEX),
            [0, 2918, 5190, 5193, 10161, 10186],
            "codex step starts"
        );
    }

    #[test]
    fn claude_tool_output_and_usage_updates_do_not_restart_the_step() {
        // Claude Code revises one tool call several times with no `status` of
        // its own — a title, then appended output — and sprays `usage_update`
        // between them. Only the creation and the completion are steps, so
        // the run of updates from 2856 to 3054 leaves the clock at 1911.
        assert_eq!(
            step_starts(CLAUDE),
            [0, 1500, 1911, 3081, 7427, 7455, 8117, 16543],
            "claude step starts"
        );
    }

    #[test]
    fn kimi_streaming_tool_input_does_not_restart_the_step() {
        // Kimi streams a tool call's arguments as repeated `in_progress`
        // updates (4309, 4437, 4437) and later revises its title while still
        // `in_progress` (4858): one step, opened at 4309. The permission
        // request and the terminal it then creates are real work starting, so
        // those do restart it, and the content-only update at 4897 does not.
        // The three `Read` calls from 13079 on run in parallel, so their
        // interleaved updates are steps of their own.
        assert_eq!(
            step_starts(KIMI),
            [
                0, 3239, 4221, 4309, 4838, 4859, 5271, 9546, 13079, 13081, 13082, 13728, 14078,
                14463, 16883, 16936, 16937, 20625, 20627
            ],
            "kimi step starts"
        );
    }

    #[test]
    fn grok_tool_calls_without_a_status_start_one_pending_step() {
        // Grok announces a tool call with no `status` field and revises it
        // with a title, a kind and content but still no status. Both are
        // `pending`, so they are one step; the terminal it opens at 10370 and
        // the failure at 10401 are the next two.
        assert_eq!(
            step_starts(GROK),
            [0, 9569, 9849, 10343, 10370, 10401],
            "grok step starts"
        );
    }

    #[test]
    fn deepseek_runs_parallel_tool_calls_as_separate_steps() {
        // Three calls are announced, started and completed as interleaved
        // runs. Each call's own status changes are steps, and the thought
        // that follows them is another.
        assert_eq!(
            step_starts(DEEPSEEK),
            [
                0, 1633, 1678, 1709, 2437, 4601, 4602, 4603, 4613, 4619, 4621, 4623, 5636, 5788
            ],
            "deepseek step starts"
        );
    }

    #[test]
    fn a_turn_that_ends_leaves_no_step_in_flight() {
        let clock = StepClock::default();
        clock.begin_turn_at(1_000);
        assert_eq!(clock.started_at_ms(), Some(1_000));
        clock.end_turn();
        assert_eq!(clock.started_at_ms(), None);
    }

    #[test]
    fn a_new_turn_reopens_a_step_of_the_kind_the_last_turn_ended_on() {
        let chunk: SessionUpdate = serde_json::from_str(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
        )
        .expect("chunk parses");

        let clock = StepClock::default();
        clock.begin_turn_at(1_000);
        clock.observe_at(&chunk, 1_100);
        clock.end_turn();
        clock.begin_turn_at(2_000);
        clock.observe_at(&chunk, 2_100);

        assert_eq!(
            clock.started_at_ms(),
            Some(2_100),
            "the first message of a new turn opens a step even though the last turn ended on one"
        );
    }
}
