//! The tool calls the agent has open right now.
//!
//! One tracker, shared by everything that needs the answer. The worker's
//! durable relay reports it in the operational state so the daemon, the CLI
//! and the browser can see that a session blocked in a long build is working;
//! the turn stall watchdog reads the same handle so it does not fail that
//! turn. Before this existed the relay kept a private map and the watchdog
//! could not reach it, which is exactly how a healthy twenty-minute `cargo
//! nextest` run came to be failed as a stalled turn.
//!
//! Like [`crate::relay::AcpActivityClock`] and [`crate::acp::StepClock`] this
//! is process-local and deliberately outside the durable journal: the tool
//! calls a harness had open die with the harness, and a restarted worker must
//! not inherit them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};
use serde::{Deserialize, Serialize};

use crate::clock::epoch_millis;

/// One tool call the agent has open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightToolCall {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `Pending` or `InProgress`; a call in any other status is not in flight.
    pub status: ToolCallStatus,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    title: Option<String>,
    status: ToolCallStatus,
    started_at_ms: i64,
}

/// A cheap shared handle on the open tool calls. Cloning it shares the state.
#[derive(Debug, Clone, Default)]
pub struct ToolsInFlight(Arc<Mutex<BTreeMap<String, Entry>>>);

impl ToolsInFlight {
    /// Fold one session update in.
    ///
    /// Pending and in-progress have portable ACP meanings across every
    /// harness, so they are the two statuses that prove foreground work.
    /// Prose and plan updates carry no corresponding end, so they cannot
    /// safely open a call of their own.
    pub fn observe(&self, update: &SessionUpdate) {
        self.observe_at(update, epoch_millis());
    }

    /// Fold one session update in, dating a call the update opens from the
    /// step it belongs to rather than from the moment it was read. The step
    /// clock is the better start: it is when the agent began this piece of
    /// work, not when the relay got round to recording it.
    pub fn observe_with_start(&self, update: &SessionUpdate, started_at_ms: i64) {
        self.observe_at(update, started_at_ms);
    }

    /// Open a call that the harness never reported as a tool call, such as a
    /// terminal Mjolnir runs on the agent's behalf.
    pub fn open(&self, tool_call_id: impl Into<String>, started_at_ms: i64) {
        self.entries().insert(
            tool_call_id.into(),
            Entry {
                title: None,
                status: ToolCallStatus::InProgress,
                started_at_ms,
            },
        );
    }

    pub fn close(&self, tool_call_id: &str) {
        self.entries().remove(tool_call_id);
    }

    /// Every open call, oldest first, so the first is the one whose age bounds
    /// the turn.
    #[must_use]
    pub fn snapshot(&self) -> Vec<InFlightToolCall> {
        let mut calls = self
            .entries()
            .iter()
            .map(|(tool_call_id, entry)| InFlightToolCall {
                tool_call_id: tool_call_id.clone(),
                title: entry.title.clone(),
                status: entry.status,
                started_at_ms: entry.started_at_ms,
            })
            .collect::<Vec<_>>();
        calls.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.tool_call_id.cmp(&right.tool_call_id))
        });
        calls
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    /// The newest open call's start, which is what the operational state has
    /// always published as `foreground_tool_started_at_ms`.
    #[must_use]
    pub fn newest_started_at_ms(&self) -> Option<i64> {
        self.entries()
            .values()
            .map(|entry| entry.started_at_ms)
            .max()
    }

    /// The harness that owned these calls is gone, and so are they.
    pub fn clear(&self) {
        self.entries().clear();
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.0.lock().expect("tools in flight lock poisoned")
    }

    fn observe_at(&self, update: &SessionUpdate, now_ms: i64) {
        let (tool_call_id, status, title) = match update {
            SessionUpdate::ToolCall(call) => (
                call.tool_call_id.0.as_ref(),
                Some(call.status),
                Some(call.title.clone()),
            ),
            SessionUpdate::ToolCallUpdate(call) => {
                (call.tool_call_id.0.as_ref(), call.fields.status, None)
            }
            _ => return,
        };
        let Some(status) = status else {
            return;
        };
        let mut entries = self.entries();
        if matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress) {
            entries
                .entry(tool_call_id.to_owned())
                .and_modify(|entry| {
                    if title.is_some() {
                        entry.title.clone_from(&title);
                    }
                    // A call that moves from pending to in-progress has begun
                    // a new step, so its clock restarts; repeated updates
                    // under an unchanged status are the same work going on.
                    if entry.status != status {
                        entry.status = status;
                        entry.started_at_ms = now_ms;
                    }
                })
                .or_insert(Entry {
                    title,
                    status,
                    started_at_ms: now_ms,
                });
        } else {
            entries.remove(tool_call_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, status: ToolCallStatus) -> SessionUpdate {
        serde_json::from_value(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": id,
            "status": match status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::InProgress => "in_progress",
                ToolCallStatus::Completed => "completed",
                ToolCallStatus::Failed => "failed",
                _ => "completed",
            },
        }))
        .expect("tool call update")
    }

    #[test]
    fn open_calls_are_listed_oldest_first_and_closed_calls_disappear() {
        let tools = ToolsInFlight::default();
        tools.observe_at(&call("first", ToolCallStatus::InProgress), 1_000);
        tools.observe_at(&call("second", ToolCallStatus::InProgress), 2_000);
        let open = tools.snapshot();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].tool_call_id, "first");
        assert_eq!(open[0].started_at_ms, 1_000);
        assert_eq!(tools.newest_started_at_ms(), Some(2_000));

        tools.observe_at(&call("first", ToolCallStatus::Completed), 3_000);
        let open = tools.snapshot();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].tool_call_id, "second");

        tools.clear();
        assert!(tools.is_empty());
    }

    #[test]
    fn a_status_change_restarts_the_clock_but_a_repeat_does_not() {
        let tools = ToolsInFlight::default();
        tools.observe_at(&call("one", ToolCallStatus::Pending), 1_000);
        tools.observe_at(&call("one", ToolCallStatus::Pending), 5_000);
        assert_eq!(tools.snapshot()[0].started_at_ms, 1_000);
        tools.observe_at(&call("one", ToolCallStatus::InProgress), 9_000);
        assert_eq!(tools.snapshot()[0].started_at_ms, 9_000);
    }
    #[test]
    fn status_updates_preserve_the_original_tool_title() {
        let tools = ToolsInFlight::default();
        tools.observe_at(&call("Build", ToolCallStatus::Pending), 1_000);
        let update = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "Build",
            "status": "in_progress", "title": "Changed title",
        }))
        .unwrap();
        tools.observe_at(&update, 2_000);
        assert_eq!(tools.snapshot()[0].title.as_deref(), Some("Build"));
        let legacy: InFlightToolCall = serde_json::from_value(serde_json::json!({
            "tool_call_id":"legacy", "status":"in_progress", "started_at_ms":1000,
        }))
        .unwrap();
        assert!(legacy.title.is_none());
    }
}
