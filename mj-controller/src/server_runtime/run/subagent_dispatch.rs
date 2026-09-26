//! Scheduling only: requests remain owned by the parent's durable worker queue.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use mj_core::subagent::{SubagentToolAction, SubagentToolRequest};

type Identity = (String, String);

#[derive(Default)]
pub(super) struct SubagentDispatch {
    pending: BTreeMap<Identity, SubagentToolRequest>,
    active: BTreeMap<Identity, Option<String>>,
    retry_at: BTreeMap<Identity, Instant>,
}

impl SubagentDispatch {
    pub fn retain_parents(&mut self, live: impl Fn(&str) -> bool) {
        self.pending.retain(|(parent, _), _| live(parent));
        self.retry_at.retain(|id, _| self.pending.contains_key(id));
    }

    pub fn observe(&mut self, parent: &str, requests: &[SubagentToolRequest]) {
        self.pending.retain(|(id, _), _| id != parent);
        for request in requests {
            self.pending.insert(
                (parent.to_owned(), request.request_id.clone()),
                request.clone(),
            );
        }
        self.retry_at.retain(|id, _| self.pending.contains_key(id));
    }

    pub fn ready(&mut self, now: Instant) -> Vec<(String, SubagentToolRequest)> {
        let mut ordered = self.pending.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(id, request)| (request.created_at_ms, *id));
        let mut children = self
            .active
            .iter()
            .filter_map(|((parent, _), child)| {
                child.as_ref().map(|child| (parent.clone(), child.clone()))
            })
            .collect::<BTreeSet<_>>();
        let mut ready = Vec::new();
        for (identity, request) in ordered {
            let child = match &request.action {
                SubagentToolAction::SendInput {
                    child_session_id, ..
                } => {
                    // Even a delayed head blocks later inputs to this child.
                    if !children.insert((identity.0.clone(), child_session_id.clone())) {
                        continue;
                    }
                    Some(child_session_id.clone())
                }
                _ => None,
            };
            if self.active.contains_key(identity)
                || self.retry_at.get(identity).is_some_and(|at| *at > now)
            {
                continue;
            }
            self.active.insert(identity.clone(), child);
            ready.push((identity.0.clone(), request.clone()));
        }
        ready
    }

    pub fn finish(&mut self, identity: &Identity, delivered: bool) {
        self.active.remove(identity);
        if delivered {
            self.pending.remove(identity);
            self.retry_at.remove(identity);
        } else {
            self.retry_at
                .insert(identity.clone(), Instant::now() + Duration::from_secs(1));
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
        let ids = |ready: Vec<(String, SubagentToolRequest)>| {
            ready
                .into_iter()
                .map(|(_, r)| r.request_id)
                .collect::<Vec<_>>()
        };
        let now = Instant::now();
        assert_eq!(ids(queue.ready(now)), ["first", "other", "interrupt"]);
        assert!(queue.ready(now).is_empty());
        queue.finish(&("p".into(), "first".into()), false);
        assert!(queue.ready(now).is_empty());
        assert_eq!(ids(queue.ready(now + Duration::from_secs(2))), ["first"]);
        queue.finish(&("p".into(), "first".into()), true);
        assert_eq!(ids(queue.ready(now)), ["second"]);
    }
}
