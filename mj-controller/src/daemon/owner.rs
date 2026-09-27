use super::record_index::RecordIndexes;
use super::*;

#[derive(Clone, PartialEq)]
pub(super) struct PollableWorkerInputs {
    ids: Vec<String>,
    config: Config,
    records: mj_core::state::State,
    moves: mj_core::snapshot_map::SnapshotMap<String, mj_core::state::MoveOperation>,
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
                crate::pollers::worker_poll_target(
                    &controller,
                    &controller.state.sessions[id],
                    Ok(self.moves.get(id).cloned()),
                )
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
    store: StoreState,
}

enum StoreState {
    Bootstrap,
    Current(crate::database::CommittedState),
    Unavailable(Arc<str>),
}

impl RuntimeStateOwner {
    pub(super) fn committed(&self) -> Option<&crate::database::CommittedState> {
        match &self.store {
            StoreState::Current(committed) => Some(committed),
            StoreState::Bootstrap | StoreState::Unavailable(_) => None,
        }
    }
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
            store: StoreState::Bootstrap,
        }
    }

    pub(super) fn install_controller(&mut self, controller: Controller) {
        if !matches!(self.store, StoreState::Bootstrap) {
            self.controller.config = controller.config;
            return;
        }
        self.indexes
            .apply(&self.controller.state, &controller.state);
        self.controller = controller;
    }

    fn observe_committed(&mut self, committed: &crate::database::CommittedState) {
        if self
            .committed()
            .is_some_and(|current| current.sequence == committed.sequence)
        {
            return;
        }
        self.indexes.apply(&self.controller.state, &committed.state);
        self.controller.state = committed.state.clone();
        self.store = StoreState::Current(committed.clone());
    }

    pub(super) fn ensure_available(&self) -> Result<()> {
        if let StoreState::Unavailable(error) = &self.store {
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
        let mut moves = mj_core::snapshot_map::SnapshotMap::new();
        for id in &ids {
            if let Some(operation) = self
                .committed()
                .and_then(|committed| committed.moves.get(id))
            {
                moves.insert(id.clone(), operation.clone());
            }
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
            moves,
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
                Err(error) => owner.store = StoreState::Unavailable(error.clone()),
            }
        }
        owner
    }
}
