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

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{SessionUpdate, ToolCallStatus};
use serde::{Deserialize, Serialize};

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

/// Process-local ownership. This never forms part of a relay snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForegroundOwner {
    Prompt(String),
    HarnessTurn(u64),
    HostedTerminal(String),
    Unowned,
}

#[derive(Debug)]
struct Entry {
    owner: ForegroundOwner,
    title: Option<String>,
    status: ToolCallStatus,
    started_at_ms: i64,
}

#[derive(Debug, Default)]
struct CallGeneration {
    entries: BTreeMap<String, Entry>,
    retired_ids: HashSet<String>,
}

/// A cheap shared handle on the open tool calls. Cloning it shares the state.
#[derive(Debug, Clone, Default)]
pub struct ToolsInFlight(Arc<Mutex<CallGeneration>>);

impl ToolsInFlight {
    pub fn observe_for_owner(
        &self,
        update: &SessionUpdate,
        started_at_ms: i64,
        owner: ForegroundOwner,
    ) {
        let (id, status, title, announced) = match update {
            SessionUpdate::ToolCall(call) => (
                call.tool_call_id.0.as_ref(),
                Some(call.status),
                Some(&call.title),
                true,
            ),
            SessionUpdate::ToolCallUpdate(update) => (
                update.tool_call_id.0.as_ref(),
                update.fields.status,
                update.fields.title.as_ref(),
                false,
            ),
            _ => return,
        };
        let mut generation = self.generation();
        if status.is_some_and(|status| {
            !matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress)
        }) {
            if generation.entries.remove(id).is_some() || announced {
                generation.retired_ids.insert(id.to_owned());
            }
            return;
        }
        if let Some(entry) = generation.entries.get_mut(id) {
            if let Some(title) = title {
                entry.title = Some(title.clone());
            }
            // The transition from pending to running is a new step.
            if let Some(status) = status
                && status != entry.status
            {
                entry.status = status;
                entry.started_at_ms = started_at_ms;
            }
        } else if announced && !generation.retired_ids.contains(id) {
            generation.entries.insert(
                id.to_owned(),
                Entry {
                    owner,
                    title: title.cloned(),
                    status: status.expect("a tool announcement has a status"),
                    started_at_ms,
                },
            );
        }
    }

    pub fn adopt_unowned(&self, owner: ForegroundOwner) {
        for entry in self.generation().entries.values_mut() {
            if entry.owner == ForegroundOwner::Unowned {
                entry.owner = owner.clone();
            }
        }
    }

    pub fn retire_owner(&self, owner: &ForegroundOwner) {
        let CallGeneration {
            entries,
            retired_ids,
        } = &mut *self.generation();
        entries.retain(|id, entry| {
            if &entry.owner == owner {
                retired_ids.insert(id.clone());
                false
            } else {
                true
            }
        });
    }

    pub fn retire_terminal(&self, terminal_id: &str) {
        self.retire_owner(&ForegroundOwner::HostedTerminal(terminal_id.to_owned()));
    }

    /// Every open call, oldest first, so the first is the one whose age bounds
    /// the turn.
    #[must_use]
    pub fn snapshot(&self) -> Vec<InFlightToolCall> {
        let mut calls = self
            .generation()
            .entries
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
        self.generation().entries.is_empty()
    }

    pub fn is_retired(&self, tool_call_id: &str) -> bool {
        self.generation().retired_ids.contains(tool_call_id)
    }

    /// Start an empty generation when the harness connection is retired.
    pub fn clear(&self) {
        *self.generation() = CallGeneration::default();
    }

    fn generation(&self) -> std::sync::MutexGuard<'_, CallGeneration> {
        self.0.lock().expect("tools in flight lock poisoned")
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
    fn a_status_change_restarts_the_clock_but_a_repeat_does_not() {
        let tools = ToolsInFlight::default();
        tools.observe_for_owner(
            &call("one", ToolCallStatus::Pending),
            1_000,
            ForegroundOwner::Unowned,
        );
        tools.observe_for_owner(
            &call("one", ToolCallStatus::Pending),
            5_000,
            ForegroundOwner::Unowned,
        );
        assert_eq!(tools.snapshot()[0].started_at_ms, 1_000);
        tools.observe_for_owner(
            &call("one", ToolCallStatus::InProgress),
            9_000,
            ForegroundOwner::Unowned,
        );
        assert_eq!(tools.snapshot()[0].started_at_ms, 9_000);
    }
}
