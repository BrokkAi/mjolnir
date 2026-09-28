//! At most one transcript per observed session, retaining the completion edge.
use super::*;

#[derive(Default)]
pub(super) struct Observations {
    running: BTreeMap<String, bool>,
    completed: BTreeMap<String, u64>,
    pending: BTreeMap<String, HostEvent>,
    ready: std::collections::VecDeque<String>,
    live: Option<BTreeSet<String>>,
}
impl Observations {
    pub(super) fn retain(&mut self, live: BTreeSet<String>) {
        self.running.retain(|id, _| live.contains(id));
        self.completed.retain(|id, _| live.contains(id));
        self.pending.retain(|id, _| live.contains(id));
        self.ready.retain(|id| live.contains(id));
        self.live = Some(live);
    }
    pub(super) fn observe(&mut self, id: &str, view: &ManagedSessionView) {
        let snapshot = view.snapshot.as_ref();
        let ordinary_prompt = |session: &MaterializedSession, command_id: &str| {
            let user_id = format!("user:{command_id}");
            !session
                .transcript
                .iter()
                .find(|item| item.stable_id == user_id)
                .is_some_and(|item| match &item.body {
                    mj_core::state::TranscriptBody::User { content } => matches!(
                        mj_core::acp::context_command_text(
                            &mj_core::transcript::materialized_content_text(content)
                        ),
                        Some((mj_core::acp::ContextCommand::Compact, _))
                    ),
                    _ => false,
                })
        };
        let prompt_driven = snapshot.is_some_and(|s| {
            s.operational
                .active_prompt
                .as_ref()
                .is_some_and(|prompt| ordinary_prompt(&s.materialized, &prompt.command_id))
        });
        // A coalescing upstream may skip Running entirely; the completed
        // ordinal is a durable fact and survives that missing observation.
        let completed = snapshot.and_then(|s| {
            s.materialized
                .last_turn_outcome
                .as_ref()
                .map(|turn| (s, turn))
        });
        let advanced = completed.is_some_and(|(s, turn)| {
            let previous = self.completed.insert(id.into(), turn.completed_ordinal);
            previous.is_some_and(|old| old < turn.completed_ordinal)
                && ordinary_prompt(&s.materialized, &turn.command_id)
        });
        let running = prompt_driven
            && snapshot.is_some_and(|s| {
                matches!(
                    s.materialized.execution,
                    MaterializedExecutionState::Running { .. }
                )
            });
        let was_running = self.running.insert(id.into(), running).unwrap_or(false);
        let finished = was_running
            && snapshot.is_some_and(|s| {
                matches!(s.materialized.execution, MaterializedExecutionState::Idle)
            });
        let retained_completion = matches!(
            self.pending.get(id),
            Some(HostEvent::View {
                finished_turn: true,
                ..
            })
        );
        let event = HostEvent::View {
            session_id: id.into(),
            snapshot: snapshot.map(|s| Box::new(s.materialized.clone())),
            prompt_driven,
            finished_turn: finished || advanced || retained_completion,
        };
        if self.pending.insert(id.into(), event).is_none() {
            self.ready.push_back(id.into());
        }
    }
    pub(super) fn take(&mut self, id: &str) -> Option<HostEvent> {
        self.pending.remove(id)
    }
    pub(super) fn pop(&mut self) -> Option<HostEvent> {
        if let Some(live) = self.live.take() {
            return Some(HostEvent::Retain { live });
        }
        while let Some(id) = self.ready.pop_front() {
            if let Some(event) = self.pending.remove(&id) {
                return Some(event);
            }
        }
        None
    }
}
