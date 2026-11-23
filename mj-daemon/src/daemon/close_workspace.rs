use super::*;

struct WorkspaceCloseGuard {
    state: Arc<RuntimeState>,
    workspace_id: String,
}

impl Drop for WorkspaceCloseGuard {
    fn drop(&mut self) {
        self.state
            .workspace_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.workspace_id);
    }
}

/// Only roots are submitted: normal close already stops their active children.
fn close_roots(state: &mj_core::state::State, workspace_id: &str) -> Vec<String> {
    let active: BTreeSet<_> = state
        .sessions
        .values()
        .filter(|session| session.workspace_id == workspace_id && session.state.is_active())
        .map(|session| session.id.clone())
        .collect();
    active
        .iter()
        .filter(|id| {
            state
                .environment_owner_id(id)
                .is_none_or(|owner| owner == id.as_str() || !active.contains(owner))
        })
        .cloned()
        .collect()
}

pub(super) async fn pmap_sessions<F: std::future::Future<Output = Result<()>> + Send + 'static>(
    ids: Vec<String>,
    operation: impl Fn(String) -> F,
    dispatched: impl FnOnce(),
    while_waiting: impl std::future::Future<Output = std::convert::Infallible>,
) -> Vec<String> {
    let mut jobs = tokio::task::JoinSet::new();
    for id in ids {
        let task = operation(id.clone());
        jobs.spawn(async move { task.await.with_context(|| format!("session {id}")) });
    }
    dispatched();
    tokio::pin!(while_waiting);
    let mut failures = Vec::new();
    while !jobs.is_empty() {
        tokio::select! {
            result = jobs.join_next() => match result {
                Some(Ok(Ok(()))) => {},
                Some(Ok(Err(error))) => failures.push(format!("{error:#}")),
                Some(Err(error)) => failures.push(format!("session task failed: {error}")),
                None => break,
            },
            impossible = &mut while_waiting => match impossible {},
        }
    }
    failures
}

impl RuntimeState {
    pub(super) fn workspace_resume_gate(&self, workspace_id: &str) -> Arc<tokio::sync::RwLock<()>> {
        self.workspace_resume_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(workspace_id.to_owned())
            .or_default()
            .clone()
    }

    pub(crate) fn cancel_workspace_close(&self, workspace_id: &str) -> Result<()> {
        let closes = self
            .workspace_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cancelled = closes
            .get(workspace_id)
            .context("workspace close is no longer running")?;
        cancelled.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn close_workspace(self: &Arc<Self>, workspace_id: String) -> Result<()> {
        // Hold handoff admission only while recording intent and dispatching
        // suspends, not while waiting for them to finish.
        let upgrade_work = mj_controller::upgrade::activity("workspace close")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut closes = self
                .workspace_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ensure!(
                !closes.contains_key(&workspace_id),
                "workspace is already closing"
            );
            closes.insert(workspace_id.clone(), cancelled.clone());
        }
        let _guard = WorkspaceCloseGuard {
            state: self.clone(),
            workspace_id: workspace_id.clone(),
        };
        ensure!(
            !self.workspace_has_active_resume(&workspace_id),
            "workspace has a session resume in progress"
        );
        let (roots, sessions) = blocking({
            let workspace_id = workspace_id.clone();
            move || {
                let controller = Controller::load()?;
                let sessions = controller
                    .state
                    .sessions
                    .values()
                    .filter(|session| {
                        session.workspace_id == workspace_id && session.state.is_active()
                    })
                    .map(|session| session.id.clone())
                    .collect::<Vec<_>>();
                Ok((close_roots(&controller.state, &workspace_id), sessions))
            }
        })
        .await?;
        let failures = pmap_sessions(
            roots,
            {
                let state = self.clone();
                let cancelled = cancelled.clone();
                move |session_id| {
                    let state = state.clone();
                    let cancelled = cancelled.clone();
                    async move {
                        ensure!(!cancelled.load(Ordering::Acquire), "workspace close cancelled");
                        state.suspend_workspace_member(session_id).await
                    }
                }
            },
            move || drop(upgrade_work),
            async {
                let mut poll = tokio::time::interval(Duration::from_millis(100));
                loop {
                    poll.tick().await;
                    if cancelled.load(Ordering::Acquire) {
                        for session_id in &sessions {
                            if let Err(error) = self.cancel_lifecycle_with_intent(session_id).await {
                                tracing::debug!(%session_id, %error, "workspace close cancellation cannot interrupt this session");
                            }
                        }
                    }
                }
            },
        ).await;
        // Once the person cancels, a session that stopped before its commit
        // did what was asked, and one past it finished. Either way the answer
        // is the cancellation; each session's record says how its own close
        // ended.
        if cancelled.load(Ordering::Acquire) {
            if !failures.is_empty() {
                tracing::info!(
                    %workspace_id,
                    sessions = failures.join("; "),
                    "workspace close was cancelled; these sessions did not stop"
                );
            }
            bail!(
                "Workspace deletion cancelled; workspace and drafts retained. Completed suspensions were not undone."
            );
        }
        ensure!(
            failures.is_empty(),
            "Workspace retained; some sessions may already be stopped. Retry to close remaining sessions: {}",
            failures.join("; ")
        );
        // A resumed session may still have its old workspace id in storage.
        // Exclude admission until the deletion transaction has committed.
        let _admission = self
            .workspace_resume_gate(&workspace_id)
            .try_write_owned()
            .context("a session resume is in progress; workspace retained, retry closing it")?;
        ensure!(
            !self.workspace_has_active_resume(&workspace_id),
            "workspace has a session resume in progress"
        );
        blocking({
            let workspace_id = workspace_id.clone();
            let cancelled = cancelled.clone();
            move || {
                ensure!(
                    !cancelled.load(Ordering::Acquire),
                    "workspace close cancelled; workspace retained"
                );
                // The transaction rejects new active sessions; insertion triggers
                // reject sessions registered after their workspace disappears.
                mj_controller::database::close_workspace(&workspace_id)
            }
        })
        .await?;
        refresh_runtime_workspaces(self).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_roots_do_not_submit_children_twice_or_touch_other_workspaces() {
        let mut state = mj_core::state::State::default();
        for (id, workspace, status) in [
            ("parent", "a", SessionState::Running),
            ("child", "a", SessionState::Running),
            ("independent", "a", SessionState::Running),
            ("owner-child", "a", SessionState::Parked),
            ("history", "a", SessionState::Stopped),
            ("other", "b", SessionState::Running),
        ] {
            let session = super::super::tests::runtime_test_session(id, workspace, status);
            state.sessions.insert(id.into(), session);
            state.environment_owners.insert(id.into(), id.into());
        }
        let child = super::super::tests::runtime_test_subagent("child", "parent");
        state.subagents.insert("child".into(), child.into());
        state
            .environment_owners
            .insert("child".into(), "parent".into());
        let owned = super::super::tests::runtime_test_subagent("owner-child", "parent");
        state.subagents.insert("owner-child".into(), owned.into());
        assert_eq!(
            close_roots(&state, "a"),
            ["independent", "owner-child", "parent"]
        );
        state.sessions.get_mut("parent").unwrap().state = SessionState::Stopped;
        assert_eq!(
            close_roots(&state, "a"),
            ["child", "independent", "owner-child"]
        );
    }
}
