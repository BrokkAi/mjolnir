use super::record_index::RecordIndexes;
use super::*;

#[derive(Clone)]
pub(super) struct PollableWorkerInputs {
    ids: Vec<String>,
    config: Config,
    records: mj_core::state::State,
    moves: mj_core::snapshot_map::SnapshotMap<String, mj_core::state::MoveOperation>,
}

struct WorkerInputsPublication {
    records: mj_core::state::State,
    moves: mj_core::snapshot_map::SnapshotMap<String, mj_core::state::MoveOperation>,
    inputs: Arc<PollableWorkerInputs>,
}

impl PollableWorkerInputs {
    pub(super) fn staged_credentials(&self) -> BTreeSet<String> {
        let mut staged = crate::pollers::staged_credential_sessions(&self.controller());
        // Ancestors describe command routing, while the owner authorizes IDs.
        staged.retain(|id| self.ids.binary_search(id).is_ok());
        staged
    }

    pub(super) fn prepare_credentials(
        &self,
        schemes: &BTreeMap<String, bool>,
        staged: &BTreeSet<String>,
    ) -> Vec<mj_core::credentials::CredentialSyncTarget> {
        crate::pollers::credential_sync_targets_from_sources(&self.controller(), schemes, staged)
    }
    pub(super) fn controller(&self) -> Controller {
        Controller {
            config: self.config.clone(),
            state: self.records.clone(),
        }
    }

    pub(super) fn prepare(&self) -> Vec<crate::session_manager::RelaySessionTarget> {
        let controller = self.controller();
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
    pub(super) sessions: mj_core::snapshot_map::SnapshotMap<String, RuntimeSessionView>,
    /// The transcript tail of every view in `sessions` that has a projection.
    /// Only [`Self::publish_view`] and the removals beside `sessions` change
    /// it, so the two never disagree about which sessions are published.
    pub(super) transcripts:
        mj_core::snapshot_map::SnapshotMap<String, mj_client::runtime_feed::SessionTail>,
    /// Sessions the session manager is told to run a relay actor for. Only
    /// these hold a view in `sessions`: a retired actor publishes no final
    /// view, so without this a stopped session kept its last live one.
    relay_sessions: BTreeSet<String>,
    pub(super) background_policies: BTreeMap<String, snapshot::BackgroundPolicyState>,
    completed: VecDeque<(String, String)>,
    store: StoreState,
    credential_inputs: Option<(Arc<PollableWorkerInputs>, BTreeSet<String>)>,
    worker_inputs: std::cell::RefCell<Option<WorkerInputsPublication>>,
}

enum StoreState {
    Bootstrap,
    Current(Arc<crate::database::CommittedState>),
    Unavailable(Arc<str>),
}

impl RuntimeStateOwner {
    pub(super) fn install_credentials(
        &mut self,
        inputs: &Arc<PollableWorkerInputs>,
        closing: &BTreeSet<String>,
        mut targets: Vec<mj_core::credentials::CredentialSyncTarget>,
        publication: &tokio::sync::watch::Sender<Vec<mj_core::credentials::CredentialSyncTarget>>,
    ) -> bool {
        if !Arc::ptr_eq(&self.pollable_worker_inputs(), inputs) || self.close_requested != *closing
        {
            return false;
        }
        targets.retain(|target| !closing.contains(&target.session_id));
        self.credential_inputs = Some((inputs.clone(), closing.clone()));
        publication.send_if_modified(|current| {
            if *current == targets {
                false
            } else {
                *current = targets;
                true
            }
        });
        true
    }

    pub(super) fn projected_records(
        &self,
    ) -> mj_core::snapshot_map::SnapshotMap<String, SessionRecord> {
        let mut records = self.controller.state.sessions.clone();
        for id in &self.close_requested {
            if let Some(record) = records.get_mut(id)
                && record.state != SessionState::Stopped
            {
                record.state = SessionState::Closing;
            }
        }
        records
    }
    pub(super) fn committed(&self) -> Option<&crate::database::CommittedState> {
        match &self.store {
            StoreState::Current(committed) => Some(committed),
            StoreState::Bootstrap | StoreState::Unavailable(_) => None,
        }
    }
    pub(super) fn controller(&self) -> &Controller {
        &self.controller
    }

    /// Record the sessions the manager now runs actors for and drop the view
    /// of every other one. Returns whether a published view went away.
    pub(super) fn install_relay_sessions(&mut self, sessions: BTreeSet<String>) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|id, _| sessions.contains(id));
        self.transcripts.retain(|id, _| sessions.contains(id));
        self.background_policies
            .retain(|id, _| sessions.contains(id));
        self.relay_sessions = sessions;
        self.sessions.len() != before
    }

    /// Whether a view for `session_id` comes from an actor the manager is
    /// still meant to run. A late view from a retired actor does not.
    pub(super) fn runs_relay_actor(&self, session_id: &str) -> bool {
        self.relay_sessions.contains(session_id)
    }

    /// Record the view a relay actor published, and bring the session's
    /// transcript tail to it. A view without a projection has no tail.
    pub(super) fn publish_view(&mut self, session_id: String, view: ManagedSessionView) {
        match view.snapshot.as_ref() {
            Some(snapshot) => match self.transcripts.get_mut(&session_id) {
                Some(tail) => tail.publish(
                    &snapshot.materialized,
                    &snapshot.window,
                    crate::database::PROJECTION_TAIL_ITEMS,
                ),
                None => {
                    self.transcripts.insert(
                        session_id.clone(),
                        mj_client::runtime_feed::SessionTail::of(
                            &snapshot.materialized,
                            &snapshot.window,
                            crate::database::PROJECTION_TAIL_ITEMS,
                        ),
                    );
                }
            },
            None => {
                self.transcripts.remove(&session_id);
            }
        }
        self.sessions.insert(
            session_id.clone(),
            RuntimeSessionView::from_managed(session_id, view),
        );
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
        self.records_changed(&before);
        result
    }

    pub(super) fn new(controller: Controller) -> Self {
        let indexes = RecordIndexes::bootstrap(&controller.state);
        Self {
            controller,
            lifecycle: BTreeMap::new(),
            close_requested: BTreeSet::new(),
            indexes,
            sessions: Default::default(),
            transcripts: Default::default(),
            relay_sessions: BTreeSet::new(),
            background_policies: BTreeMap::new(),
            completed: VecDeque::new(),
            store: StoreState::Bootstrap,
            credential_inputs: None,
            worker_inputs: Default::default(),
        }
    }

    pub(super) fn install_controller(&mut self, controller: Controller) {
        if !matches!(self.store, StoreState::Bootstrap) {
            self.controller.config = controller.config;
            return;
        }
        let before = self.controller.state.clone();
        self.controller = controller;
        self.records_changed(&before);
    }

    fn observe_committed(&mut self, committed: &Arc<crate::database::CommittedState>) {
        if self
            .committed()
            .is_some_and(|current| current.sequence == committed.sequence)
        {
            return;
        }
        let before = self.controller.state.clone();
        self.controller.state = committed.state.clone();
        self.records_changed(&before);
        self.store = StoreState::Current(committed.clone());
    }

    fn records_changed(&mut self, before: &mj_core::state::State) {
        self.indexes.apply(before, &self.controller.state);
        for (id, record) in before.sessions.changes(&self.controller.state.sessions) {
            if record.is_none() {
                self.sessions.remove(id);
                self.transcripts.remove(id);
                self.background_policies.remove(id);
            }
        }
    }

    pub(super) fn ensure_available(&self) -> Result<()> {
        if let StoreState::Unavailable(error) = &self.store {
            bail!("{error}");
        }
        Ok(())
    }

    pub(super) fn complete_lifecycle(
        &mut self,
        session_id: &str,
        operation_id: &str,
        result: LifecycleResult,
    ) {
        const RETAINED_OUTCOMES: usize = 256;
        let Some(active) = self.lifecycle.get_mut(session_id) else {
            return;
        };
        if active.operation_id != operation_id || !active.is_running() {
            return;
        }
        active.phase = LifecyclePhase::Completed(result);
        active._move_guard.take();
        if !active.is_visible() {
            self.completed
                .push_back((session_id.to_owned(), operation_id.to_owned()));
        }
        while self.completed.len() > RETAINED_OUTCOMES {
            let (id, operation) = self.completed.pop_front().expect("retained outcome");
            if self
                .lifecycle
                .get(&id)
                .is_some_and(|active| active.operation_id == operation && !active.is_visible())
            {
                self.lifecycle.remove(&id);
            }
        }
    }

    pub(super) fn worker_is_owned(&self, session_id: &str) -> bool {
        self.lifecycle.get(session_id).is_some_and(|active| {
            active.is_running()
                && (matches!(
                    active.phase,
                    LifecyclePhase::MovingDestination | LifecyclePhase::CancellingMoveDestination
                ) || lifecycle_owns_worker_target(
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

    pub(super) fn pollable_worker_inputs(&self) -> Arc<PollableWorkerInputs> {
        let ids = self.pollable_worker_ids();
        let empty_moves = Default::default();
        let source_moves = self
            .committed()
            .map(|committed| &committed.moves)
            .unwrap_or(&empty_moves);
        if let Some(publication) = self.worker_inputs.borrow().as_ref()
            && publication.inputs.ids == ids
            && publication.inputs.config == self.controller.config
            && publication
                .records
                .sessions
                .changes(&self.controller.state.sessions)
                .next()
                .is_none()
            && publication
                .records
                .subagents
                .changes(&self.controller.state.subagents)
                .next()
                .is_none()
            && publication.moves.changes(source_moves).next().is_none()
        {
            return publication.inputs.clone();
        }
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
        let inputs = Arc::new(PollableWorkerInputs {
            ids,
            config: self.controller.config.clone(),
            records,
            moves,
        });
        *self.worker_inputs.borrow_mut() = Some(WorkerInputsPublication {
            records: self.controller.state.clone(),
            moves: source_moves.clone(),
            inputs: inputs.clone(),
        });
        inputs
    }
}

impl RuntimeState {
    pub(crate) fn credential_target_is_current(
        &self,
        target: &mj_core::credentials::CredentialSyncTarget,
    ) -> Result<bool> {
        let owner = self.owner();
        owner.ensure_available()?;
        Ok(owner
            .credential_inputs
            .as_ref()
            .is_some_and(|(prepared, closing)| {
                Arc::ptr_eq(prepared, &owner.pollable_worker_inputs())
                    && *closing == owner.close_requested
                    && !closing.contains(&target.session_id)
                    && self.credential_targets.borrow().contains(target)
            }))
    }

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

#[cfg(test)]
mod tests {
    use super::super::tests::runtime_test_session;
    use super::*;

    #[cfg(unix)]
    #[test]
    fn credential_staging_changes_are_observed_without_record_changes() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let original = root.path().join("original");
        std::fs::create_dir(&original).unwrap();
        std::os::unix::fs::symlink(&original, &profile).unwrap();
        let mut state = mj_core::state::State::default();
        let mut active = runtime_test_session("active", "workspace", SessionState::Running);
        active.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: root.path().to_owned(),
        });
        state.sessions.insert(active.id.clone(), active);
        let owner = RuntimeStateOwner::new(Controller {
            config: Config::default(),
            state,
        });
        let inputs = owner.pollable_worker_inputs();
        assert!(inputs.staged_credentials().is_empty());
        std::fs::remove_file(&profile).unwrap();
        std::fs::create_dir(&profile).unwrap();
        assert_eq!(
            inputs.staged_credentials(),
            BTreeSet::from(["active".into()])
        );
        assert!(Arc::ptr_eq(&inputs, &owner.pollable_worker_inputs()));
        std::fs::remove_dir(&profile).unwrap();
        std::os::unix::fs::symlink(&original, &profile).unwrap();
        assert!(inputs.staged_credentials().is_empty());
    }

    #[test]
    fn credential_preparation_cannot_install_after_ownership_or_config_changes() {
        let mut state = mj_core::state::State::default();
        let mut active = runtime_test_session("active", "workspace", SessionState::Running);
        active.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: "/worker".into(),
        });
        state.sessions.insert(active.id.clone(), active);
        let mut owner = RuntimeStateOwner::new(Controller {
            config: Config::default(),
            state,
        });
        let inputs = owner.pollable_worker_inputs();
        assert!(
            Arc::ptr_eq(&inputs, &owner.pollable_worker_inputs()),
            "unchanged input records retain publication identity"
        );
        let closing = BTreeSet::new();
        for connected in [false, true, false] {
            owner.publish_view(
                "active".into(),
                ManagedSessionView {
                    connected,
                    ..Default::default()
                },
            );
            assert!(
                Arc::ptr_eq(&inputs, &owner.pollable_worker_inputs()),
                "operational views cannot rebuild worker input records"
            );
        }
        let (publication, receiver) = tokio::sync::watch::channel(Vec::new());
        assert!(owner.install_credentials(&inputs, &closing, Vec::new(), &publication));
        assert!(
            !receiver.has_changed().unwrap(),
            "an unchanged target set publishes nothing"
        );
        owner.close_requested.insert("active".into());
        assert!(!owner.install_credentials(&inputs, &closing, Vec::new(), &publication));
        owner.close_requested.clear();
        assert!(Arc::ptr_eq(&inputs, &owner.pollable_worker_inputs()));
        owner.edit_sessions(|sessions| {
            sessions.get_mut("active").unwrap().state = SessionState::Parked;
        });
        assert!(!owner.install_credentials(&inputs, &closing, Vec::new(), &publication));
        let parked = owner.pollable_worker_inputs();
        owner.controller.config.profiles.insert(
            "new".into(),
            serde_json::from_value(serde_json::json!({"kind":"codex", "home":"/profile"})).unwrap(),
        );
        assert!(!owner.install_credentials(&parked, &closing, Vec::new(), &publication));
        assert!(
            !receiver.has_changed().unwrap(),
            "stale preparation never changes the published targets"
        );
    }

    #[test]
    fn polling_visits_no_historical_records_at_any_history_size() {
        for historical in [100, 10_000, 100_000] {
            let mut state = mj_core::state::State::default();
            for index in 0..historical {
                let id = format!("history-{index:06}");
                state.sessions.insert(
                    id.clone(),
                    runtime_test_session(&id, "workspace", SessionState::Stopped),
                );
            }
            let mut active = runtime_test_session("active", "workspace", SessionState::Running);
            active.target = Some(mj_core::state::TargetLocator::LocalBare {
                worker_root: "/worker".into(),
            });
            state.sessions.insert(active.id.clone(), active.clone());
            let mut owner = RuntimeStateOwner::new(Controller {
                config: Config::default(),
                state,
            });
            crate::pollers::take_pollability_visits();
            let held = owner.pollable_worker_inputs();
            assert!(Arc::ptr_eq(&held, &owner.pollable_worker_inputs()));
            assert_eq!(held.ids, ["active"]);
            assert_eq!(held.records.sessions.len(), 1);
            assert_eq!(
                crate::pollers::take_pollability_visits(),
                0,
                "polling re-evaluated session history"
            );
            owner.edit_sessions(|sessions| {
                let record = sessions.get_mut("history-000000").unwrap();
                record.state = SessionState::Running;
                record.target = active.target.clone();
            });
            assert_eq!(
                crate::pollers::take_pollability_visits(),
                2,
                "only the changed record's before and after eligibility may be evaluated"
            );
            assert_eq!(
                owner.pollable_worker_inputs().ids,
                ["active", "history-000000"]
            );
            assert_eq!(
                held.records.sessions.len(),
                1,
                "a retained query snapshot changed"
            );
            assert_eq!(crate::pollers::take_pollability_visits(), 0);
        }
    }
}
