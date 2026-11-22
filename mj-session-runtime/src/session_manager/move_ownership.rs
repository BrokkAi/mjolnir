//! Which sessions a Move owns. The Move operations in the controller reserve
//! and release ownership here; the session actors and worker recovery ask
//! here before they act on a session, so one record answers both.

use anyhow::{Result, ensure};
use mj_core::relay::RelayCommand;

/// Execution and retained queue admission are phases of one ownership record.
#[derive(Clone, Copy)]
enum MoveOwnership {
    Executing,
    ExecutingQueue,
    PendingQueue,
}

fn move_ownership() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, MoveOwnership>>
{
    static OWNER: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, MoveOwnership>>,
    > = std::sync::OnceLock::new();
    OWNER.get_or_init(Default::default)
}

pub fn move_owns_session(session_id: &str) -> bool {
    move_ownership()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(session_id)
}

pub fn move_has_pending_queue(session_id: &str) -> bool {
    matches!(
        move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id),
        Some(MoveOwnership::ExecutingQueue | MoveOwnership::PendingQueue)
    )
}

pub fn release_move_queue_hold(session_id: &str) {
    set_move_queue_hold(session_id, false);
}

pub fn set_move_queue_hold(session_id: &str, pending: bool) {
    let mut owner = move_ownership()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = match (owner.get(session_id), pending) {
        (Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue), true) => {
            Some(MoveOwnership::ExecutingQueue)
        }
        (Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue), false) => {
            Some(MoveOwnership::Executing)
        }
        (_, true) => Some(MoveOwnership::PendingQueue),
        (_, false) => None,
    };
    if let Some(next) = next {
        owner.insert(session_id.to_owned(), next);
    } else {
        owner.remove(session_id);
    }
}

pub struct MoveMutationGuard(String);

impl MoveMutationGuard {
    pub fn reserve(session_id: &str) -> Result<Self> {
        let mut owner = move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            !matches!(
                owner.get(session_id),
                Some(MoveOwnership::Executing | MoveOwnership::ExecutingQueue)
            ),
            "session already has a move owner"
        );
        let next = if owner.contains_key(session_id) {
            MoveOwnership::ExecutingQueue
        } else {
            MoveOwnership::Executing
        };
        owner.insert(session_id.to_owned(), next);
        Ok(Self(session_id.to_owned()))
    }
}

impl Drop for MoveMutationGuard {
    fn drop(&mut self) {
        let mut owner = move_ownership()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match owner.get(&self.0) {
            Some(MoveOwnership::ExecutingQueue) => {
                owner.insert(self.0.clone(), MoveOwnership::PendingQueue);
            }
            Some(MoveOwnership::Executing) => {
                owner.remove(&self.0);
            }
            _ => {}
        }
    }
}

pub fn move_refuses_command(session_id: &str, command: &RelayCommand) -> bool {
    move_owns_session(session_id)
        && matches!(
            command,
            RelayCommand::Prompt { .. }
                | RelayCommand::SetConfig { .. }
                | RelayCommand::SetSessionMode { .. }
                | RelayCommand::RestoreExecutionMode
                | RelayCommand::RunUserShell { .. }
                | RelayCommand::CancelUserShell { .. }
                | RelayCommand::Cancel
                | RelayCommand::CancelTurn
                | RelayCommand::CancelTurnFor { .. }
                | RelayCommand::DismissQuestion { .. }
                | RelayCommand::RemoveQueuedPrompt { .. }
                | RelayCommand::ClearQueuedPrompts
        )
}

/// Whether this launch exposes the worker's durable sub-agent request queue.
/// Top-level launches stage the message-only marker; child launches instead
/// depend on the recorded handback choice.
pub fn worker_subagent_queue_enabled(
    policy: &mj_core::subagent::SubagentPolicy,
    harness: mj_core::config::HarnessKind,
    subagent: Option<bool>,
    agent_mailboxes_enabled: bool,
) -> bool {
    let managed_subagent = subagent.is_some();
    let launch_policy = if managed_subagent {
        mj_core::subagent::SubagentPolicy::None
    } else {
        policy.for_launch(harness, None)
    };
    mj_core::worker_launch::resolve_subagent_mcp_role(
        &launch_policy,
        managed_subagent,
        false,
        subagent.unwrap_or(false),
        agent_mailboxes_enabled,
        !managed_subagent,
    )
    .is_some()
}
