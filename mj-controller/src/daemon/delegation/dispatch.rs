//! Scheduling only: requests remain owned by the parent's durable worker queue.
use crate::database::StoredDelegationResult;
use mj_core::subagent::{SubagentToolAction, SubagentToolRequest};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub(super) type Identity = (String, String);
enum Phase {
    Pending(Instant),
    Executing,
    Delivery(StoredDelegationResult, Instant),
    Delivering(StoredDelegationResult),
    Finished,
}
struct Entry {
    request: SubagentToolRequest,
    phase: Phase,
}
#[derive(Default)]
pub(super) struct SubagentDispatch {
    entries: BTreeMap<Identity, Entry>,
    available: BTreeSet<String>,
}
pub(super) enum Job {
    Execute(SubagentToolRequest),
    Deliver(StoredDelegationResult),
}
impl SubagentDispatch {
    pub fn observe(&mut self, parent: &str, requests: &[SubagentToolRequest]) {
        self.available.insert(parent.to_owned());
        self.entries.retain(|(id, request), entry| {
            id != parent
                || requests.iter().any(|r| &r.request_id == request)
                || matches!(entry.phase, Phase::Executing | Phase::Delivering(_))
        });
        for request in requests {
            self.entries
                .entry((parent.to_owned(), request.request_id.clone()))
                .or_insert_with(|| {
                    tracing::debug!(parent_session_id = parent, request_id = %request.request_id,
                        age_ms = chrono::Utc::now().timestamp_millis().saturating_sub(request.created_at_ms),
                        "delegation request observed");
                    Entry { request: request.clone(), phase: Phase::Pending(Instant::now()) }
                });
        }
    }
    pub fn retire(&mut self, parent: &str) {
        self.available.remove(parent);
        // Losing the observer does not mean the durable queue was emptied.
        // Keep accepted executions and undelivered results across replacement.
        self.entries.retain(|(id, _), entry| {
            id != parent
                || matches!(
                    entry.phase,
                    Phase::Executing | Phase::Delivery(..) | Phase::Delivering(_)
                )
        });
    }
    pub fn ready(&mut self, now: Instant) -> Vec<(Identity, Job)> {
        let mut ordered = self
            .entries
            .iter()
            .filter(|(id, _)| self.available.contains(&id.0))
            .map(|(id, e)| (e.request.created_at_ms, id.clone()))
            .collect::<Vec<_>>();
        ordered.sort();
        // Long waits and child startup cannot occupy the reserved control lane.
        let mut executing = [0usize; 2];
        let mut delivering = 0usize;
        for entry in self.entries.values() {
            match entry.phase {
                Phase::Executing => executing[execution_lane(&entry.request)] += 1,
                Phase::Delivering(_) => delivering += 1,
                _ => {}
            }
        }
        let mut children = BTreeSet::new();
        let mut ready = Vec::new();
        for (_, id) in ordered {
            let entry = self.entries.get_mut(&id).expect("pending entry");
            if matches!(entry.phase, Phase::Finished) {
                continue;
            }
            let ordered_child = match &entry.request.action {
                SubagentToolAction::SendInput {
                    child_session_id, ..
                }
                | SubagentToolAction::SendMessage {
                    child_session_id, ..
                } => Some(child_session_id),
                SubagentToolAction::Handback { .. } => Some(&id.0),
                _ => None,
            };
            if let Some(child) = ordered_child
                && !children.insert((id.0.clone(), child.clone()))
            {
                continue;
            }
            match &entry.phase {
                Phase::Pending(at)
                    if *at <= now && executing[execution_lane(&entry.request)] < 32 =>
                {
                    executing[execution_lane(&entry.request)] += 1;
                    entry.phase = Phase::Executing;
                    ready.push((id, Job::Execute(entry.request.clone())));
                }
                Phase::Delivery(result, at) if *at <= now && delivering < 32 => {
                    delivering += 1;
                    let result = result.clone();
                    entry.phase = Phase::Delivering(result.clone());
                    ready.push((id, Job::Deliver(result)));
                }
                _ => {}
            }
        }
        ready
    }
    pub fn executed(&mut self, id: &Identity, result: StoredDelegationResult) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.phase = Phase::Delivery(result, Instant::now());
        }
    }
    pub fn unaccepted(&mut self, id: &Identity) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.phase = Phase::Pending(Instant::now() + Duration::from_secs(1));
        }
    }
    pub fn delivered(&mut self, id: &Identity, success: bool) {
        if let Some(entry) = self.entries.get_mut(id) {
            if success {
                entry.phase = Phase::Finished;
            } else if let Phase::Delivering(result) | Phase::Delivery(result, _) = &entry.phase {
                entry.phase =
                    Phase::Delivery(result.clone(), Instant::now() + Duration::from_secs(1));
            }
        }
    }
    pub fn failed_task(&mut self, id: &Identity) {
        if let Some(entry) = self.entries.get(id) {
            if matches!(entry.phase, Phase::Delivering(_)) {
                self.delivered(id, false);
            } else {
                // A task failure is not an effect result. Recover its durable
                // phase on retry; never fabricate an answer after uncertain IO.
                self.unaccepted(id);
            }
        }
    }
}

fn execution_lane(request: &SubagentToolRequest) -> usize {
    usize::from(matches!(
        request.action,
        SubagentToolAction::LegacyInterruptAgent { .. } | SubagentToolAction::CloseAgent { .. }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(id: &str, child: &str, created_at_ms: i64) -> SubagentToolRequest {
        SubagentToolRequest {
            originating_command_id: None,
            request_id: id.into(),
            created_at_ms,
            action: SubagentToolAction::SendInput {
                child_session_id: child.into(),
                message: id.into(),
            },
        }
    }
    fn result(id: &str) -> StoredDelegationResult {
        StoredDelegationResult {
            result: mj_core::subagent::SubagentToolResult {
                request_id: id.into(),
                completed_at_ms: 2,
                is_error: false,
                message: "done".into(),
            },
            reported_finishes: Vec::new(),
        }
    }
    #[test]
    fn orders_child_messages_without_blocking_other_children() {
        let mut queue = SubagentDispatch::default();
        let mut message = input("message", "a", 3);
        message.action = SubagentToolAction::SendMessage {
            child_session_id: "a".into(),
            message: "note".into(),
        };
        queue.observe(
            "p",
            &[
                input("second", "a", 2),
                input("first", "a", 1),
                input("other", "b", 2),
                message,
            ],
        );
        let now = Instant::now() + Duration::from_secs(1);
        let ids =
            |ready: Vec<(Identity, Job)>| ready.into_iter().map(|(id, _)| id.1).collect::<Vec<_>>();
        assert_eq!(ids(queue.ready(now)), ["first", "other"]);
        assert!(queue.ready(now).is_empty());
        let first = ("p".into(), "first".into());
        queue.unaccepted(&first);
        assert!(queue.ready(Instant::now()).is_empty());
        assert_eq!(ids(queue.ready(now + Duration::from_secs(2))), ["first"]);
        queue.executed(&first, result("first"));
        assert_eq!(ids(queue.ready(now)), ["first"]);
        queue.delivered(&first, true);
        assert_eq!(ids(queue.ready(now)), ["second"]);
        let second = ("p".into(), "second".into());
        queue.executed(&second, result("second"));
        assert_eq!(ids(queue.ready(now)), ["second"]);
        queue.delivered(&second, true);
        assert_eq!(ids(queue.ready(now)), ["message"]);
    }
    #[test]
    fn a_lost_delivery_acknowledgement_retries_the_result_without_reexecuting() {
        let mut queue = SubagentDispatch::default();
        let request = input("first", "a", 1);
        queue.observe("p", std::slice::from_ref(&request));
        let id = ("p".into(), "first".into());
        assert!(matches!(
            queue.ready(Instant::now()).pop().unwrap().1,
            Job::Execute(_)
        ));
        queue.executed(&id, result("first"));
        assert!(matches!(
            queue.ready(Instant::now()).pop().unwrap().1,
            Job::Deliver(_)
        ));
        queue.delivered(&id, false);
        queue.observe("p", std::slice::from_ref(&request));
        assert!(queue.ready(Instant::now()).is_empty());
        assert!(matches!(
            queue
                .ready(Instant::now() + Duration::from_secs(2))
                .pop()
                .unwrap()
                .1,
            Job::Deliver(_)
        ));
        queue.delivered(&id, true);
        // A snapshot captured before delivery must not resurrect executed work.
        queue.observe("p", &[request]);
        assert!(queue.ready(Instant::now()).is_empty());
        queue.observe("p", &[]);
        assert!(queue.entries.is_empty());
    }
    #[test]
    fn replacement_daemon_rebuilds_pending_work_from_the_worker_queue() {
        let request = input("first", "a", 1);
        let mut old = SubagentDispatch::default();
        old.observe("p", std::slice::from_ref(&request));
        assert_eq!(old.ready(Instant::now()).len(), 1);
        drop(old);
        let mut replacement = SubagentDispatch::default();
        replacement.observe("p", &[request]);
        assert_eq!(
            replacement.ready(Instant::now()).pop().unwrap().0,
            ("p".into(), "first".into())
        );
    }
    #[test]
    fn actor_replacement_preserves_an_executed_result_until_delivery() {
        let mut queue = SubagentDispatch::default();
        let request = input("first", "a", 1);
        let id = ("p".into(), "first".into());
        queue.observe("p", std::slice::from_ref(&request));
        assert!(matches!(
            queue.ready(Instant::now()).pop().unwrap().1,
            Job::Execute(_)
        ));
        queue.executed(&id, result("first"));
        queue.retire("p");
        assert!(queue.ready(Instant::now()).is_empty());
        queue.observe("p", &[request]);
        assert!(matches!(
            queue.ready(Instant::now()).pop().unwrap().1,
            Job::Deliver(_)
        ));
        queue.delivered(&id, true);
        assert!(queue.ready(Instant::now()).is_empty());
        queue.observe("p", &[]);
        assert!(queue.entries.is_empty());
    }
    #[test]
    fn saturated_execution_keeps_control_and_result_delivery_available() {
        let mut queue = SubagentDispatch::default();
        let requests = (0..100)
            .map(|n| input(&format!("input-{n}"), &format!("child-{n}"), n))
            .collect::<Vec<_>>();
        queue.observe("parent", &requests);
        let first = queue.ready(Instant::now());
        assert_eq!(first.len(), 32);
        assert!(queue.ready(Instant::now()).is_empty());
        let mut requests = requests;
        let mut message = input("message", "child-100", 101);
        message.action = SubagentToolAction::SendMessage {
            child_session_id: "child-100".into(),
            message: "note".into(),
        };
        let mut close = input("close", "child-close", 102);
        close.action = SubagentToolAction::CloseAgent {
            child_session_id: "child-close".into(),
        };
        requests.push(message);
        requests.push(close);
        queue.observe("parent", &requests);
        let control = queue.ready(Instant::now());
        assert_eq!(control.len(), 1);
        assert_eq!(control[0].0.1, "close");
        queue.executed(&first[0].0, result(&first[0].0.1));
        let next = queue.ready(Instant::now());
        assert_eq!(next.len(), 2);
        assert!(next.iter().any(|(_, job)| matches!(job, Job::Deliver(_))));
    }

    #[test]
    fn failed_effect_task_retries_durable_execution_instead_of_inventing_result() {
        let mut queue = SubagentDispatch::default();
        queue.observe("parent", &[input("request", "child", 1)]);
        let id = queue.ready(Instant::now()).pop().unwrap().0;
        queue.failed_task(&id);
        assert!(queue.ready(Instant::now()).is_empty());
        assert!(matches!(
            queue
                .ready(Instant::now() + Duration::from_secs(2))
                .pop()
                .unwrap()
                .1,
            Job::Execute(_)
        ));
    }
}
