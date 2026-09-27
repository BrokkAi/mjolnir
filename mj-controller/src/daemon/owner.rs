use super::record_index::RecordIndexes;
use super::*;

#[derive(Clone, PartialEq)]
pub(super) struct PollableWorkerInputs {
    ids: Vec<String>,
    config: Config,
    records: mj_core::state::State,
}

impl PollableWorkerInputs {
    pub(super) fn prepare(&self) -> Vec<crate::session_manager::RelaySessionTarget> {
        let controller = Controller {
            config: self.config.clone(),
            state: self.records.clone(),
        };
        self.ids
            .iter()
            .filter_map(|id| {
                crate::pollers::worker_poll_target(&controller, &controller.state.sessions[id])
            })
            .collect()
    }
}

/// Records, control ownership and eligibility change under one lock. External
/// work receives copied inputs and reports its result back to this owner.
pub(super) struct RuntimeStateOwner {
    controller: Controller,
    pub(super) lifecycle: BTreeMap<String, ActiveLifecycle>,
    pub(super) close_requested: BTreeSet<String>,
    pub(super) indexes: RecordIndexes,
    committed_sequence: Option<u64>,
    pub(super) store_failure: Option<Arc<str>>,
}

impl RuntimeStateOwner {
    pub(super) fn controller(&self) -> &Controller {
        &self.controller
    }

    pub(super) fn install_config(&mut self, config: Config) {
        self.controller.config = config;
    }

    pub(super) fn edit_sessions<R>(
        &mut self,
        edit: impl FnOnce(&mut mj_core::snapshot_map::SnapshotMap<String, SessionRecord>) -> R,
    ) -> R {
        let before = self.controller.state.clone();
        let result = edit(&mut self.controller.state.sessions);
        self.indexes.apply(&before, &self.controller.state);
        result
    }

    pub(super) fn new(controller: Controller) -> Self {
        let indexes = RecordIndexes::bootstrap(&controller.state);
        Self {
            controller,
            lifecycle: BTreeMap::new(),
            close_requested: BTreeSet::new(),
            indexes,
            committed_sequence: None,
            store_failure: None,
        }
    }

    pub(super) fn install_controller(&mut self, controller: Controller) {
        if self.committed_sequence.is_some() {
            self.controller.config = controller.config;
            return;
        }
        self.indexes
            .apply(&self.controller.state, &controller.state);
        self.controller = controller;
    }

    fn observe_committed(&mut self, committed: &crate::database::CommittedState) {
        if self.committed_sequence == Some(committed.sequence) {
            return;
        }
        self.indexes.apply(&self.controller.state, &committed.state);
        self.controller.state = committed.state.clone();
        self.committed_sequence = Some(committed.sequence);
    }

    pub(super) fn ensure_available(&self) -> Result<()> {
        if let Some(error) = &self.store_failure {
            bail!("{error}");
        }
        Ok(())
    }

    pub(super) fn worker_is_owned(&self, session_id: &str) -> bool {
        self.lifecycle.get(session_id).is_some_and(|active| {
            active.is_running()
                && (active.move_source_closed
                    || lifecycle_owns_worker_target(
                        active.kind,
                        self.controller
                            .state
                            .sessions
                            .get(session_id)
                            .map(|record| record.state),
                    ))
        })
    }

    pub(super) fn pollable_worker_ids(&self) -> Vec<String> {
        self.indexes
            .pollable
            .keys()
            .filter(|id| !self.worker_is_owned(id))
            .cloned()
            .collect()
    }

    pub(super) fn pollable_worker_inputs(&self) -> PollableWorkerInputs {
        let ids = self.pollable_worker_ids();
        let mut records = mj_core::state::State::default();
        for id in &ids {
            let mut current = id.as_str();
            while !records.sessions.contains_key(current) {
                let Some(record) = self.controller.state.sessions.get(current) else {
                    break;
                };
                records.sessions.insert(current.to_owned(), record.clone());
                let Some(relation) = self.controller.state.subagents.get(current) else {
                    break;
                };
                records
                    .subagents
                    .insert(current.to_owned(), relation.clone());
                current = &relation.parent_session_id;
            }
        }
        PollableWorkerInputs {
            ids,
            config: self.controller.config.clone(),
            records,
        }
    }
}

impl RuntimeState {
    /// A record publication is visible before its writer reply. Refreshing at
    /// this boundary ensures every decision observes that publication together
    /// with control ownership, even before the background subscriber wakes up.
    pub(super) fn owner(&self) -> std::sync::MutexGuard<'_, RuntimeStateOwner> {
        let mut owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(committed) = &self.committed {
            match &*committed.borrow() {
                Ok(committed) => owner.observe_committed(committed),
                Err(error) => owner.store_failure = Some(error.clone()),
            }
        }
        owner
    }
}
