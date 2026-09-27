//! Scheduling only: requests remain owned by the parent's durable worker queue.
use mj_core::subagent::{SubagentToolAction, SubagentToolRequest, SubagentToolResult};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub(super) type Identity = (String, String);
enum Phase {
    Pending(Instant),
    Executing,
    Delivery(SubagentToolResult, Instant),
    Delivering(SubagentToolResult),
    Finished,
}
struct Entry {
    request: SubagentToolRequest,
    phase: Phase,
}
#[derive(Default)]
pub(super) struct SubagentDispatch {
    entries: BTreeMap<Identity, Entry>,
}
pub(super) enum Job {
    Execute(SubagentToolRequest),
    Deliver(SubagentToolResult),
}
impl SubagentDispatch {
    pub fn observe(&mut self, parent: &str, requests: &[SubagentToolRequest]) {
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
        self.observe(parent, &[]);
    }
    pub fn ready(&mut self, now: Instant) -> Vec<(Identity, Job)> {
        let mut ordered = self
            .entries
            .iter()
            .map(|(id, e)| (e.request.created_at_ms, id.clone()))
            .collect::<Vec<_>>();
        ordered.sort();
        let mut children = BTreeSet::new();
        let mut ready = Vec::new();
        for (_, id) in ordered {
            let entry = self.entries.get_mut(&id).expect("pending entry");
            if matches!(entry.phase, Phase::Finished) {
                continue;
            }
            if let SubagentToolAction::SendInput {
                child_session_id, ..
            } = &entry.request.action
                && !children.insert((id.0.clone(), child_session_id.clone()))
            {
                continue;
            }
            match &entry.phase {
                Phase::Pending(at) if *at <= now => {
                    entry.phase = Phase::Executing;
                    ready.push((id, Job::Execute(entry.request.clone())));
                }
                Phase::Delivery(result, at) if *at <= now => {
                    let result = result.clone();
                    entry.phase = Phase::Delivering(result.clone());
                    ready.push((id, Job::Deliver(result)));
                }
                _ => {}
            }
        }
        ready
    }
    pub fn executed(&mut self, id: &Identity, result: SubagentToolResult) {
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
    pub fn failed_task(&mut self, id: &Identity, error: String) {
        if let Some(entry) = self.entries.get_mut(id) {
            if matches!(entry.phase, Phase::Delivering(_)) {
                self.delivered(id, false);
            } else {
                self.executed(
                    id,
                    SubagentToolResult {
                        request_id: id.1.clone(),
                        completed_at_ms: chrono::Utc::now().timestamp_millis(),
                        is_error: true,
                        message: error,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(id: &str, child: &str, created_at_ms: i64) -> SubagentToolRequest {
        SubagentToolRequest {
            request_id: id.into(),
            created_at_ms,
            action: SubagentToolAction::SendInput {
                child_session_id: child.into(),
                message: id.into(),
            },
        }
    }
    fn result(id: &str) -> SubagentToolResult {
        SubagentToolResult {
            request_id: id.into(),
            completed_at_ms: 2,
            is_error: false,
            message: "done".into(),
        }
    }
    #[test]
    fn orders_child_inputs_without_blocking_other_children_or_interrupts() {
        let mut queue = SubagentDispatch::default();
        let mut interrupt = input("interrupt", "a", 3);
        interrupt.action = SubagentToolAction::InterruptAgent {
            child_session_id: "a".into(),
        };
        queue.observe(
            "p",
            &[
                input("second", "a", 2),
                input("first", "a", 1),
                input("other", "b", 2),
                interrupt,
            ],
        );
        let now = Instant::now() + Duration::from_secs(1);
        let ids =
            |ready: Vec<(Identity, Job)>| ready.into_iter().map(|(id, _)| id.1).collect::<Vec<_>>();
        assert_eq!(ids(queue.ready(now)), ["first", "other", "interrupt"]);
        assert!(queue.ready(now).is_empty());
        let first = ("p".into(), "first".into());
        queue.unaccepted(&first);
        assert!(queue.ready(Instant::now()).is_empty());
        assert_eq!(ids(queue.ready(now + Duration::from_secs(2))), ["first"]);
        queue.executed(&first, result("first"));
        assert_eq!(ids(queue.ready(now)), ["first"]);
        queue.delivered(&first, true);
        assert_eq!(ids(queue.ready(now)), ["second"]);
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
}
