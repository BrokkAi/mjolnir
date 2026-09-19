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
            !state.subagents.values().any(|child| {
                &child.child_session_id == *id && active.contains(&child.parent_session_id)
            })
        })
        .cloned()
        .collect()
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

    pub(super) fn cancel_workspace_close(&self, workspace_id: &str) -> Result<()> {
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
        let mut jobs = tokio::task::JoinSet::new();
        for session_id in roots {
            let state = self.clone();
            let cancelled = cancelled.clone();
            jobs.spawn(async move {
                ensure!(
                    !cancelled.load(Ordering::Acquire),
                    "workspace close cancelled"
                );
                state
                    .close_session(session_id.clone())
                    .await
                    .with_context(|| format!("stop session {session_id}"))
            });
        }
        let mut failures = Vec::new();
        let mut cancellation_poll = tokio::time::interval(Duration::from_millis(100));
        while !jobs.is_empty() {
            tokio::select! {
                result = jobs.join_next() => match result {
                    Some(Ok(Ok(()))) => {},
                    Some(Ok(Err(error))) => failures.push(format!("{error:#}")),
                    Some(Err(error)) => failures.push(format!("workspace close task failed: {error}")),
                    None => break,
                },
                _ = cancellation_poll.tick() => {
                    if cancelled.load(Ordering::Acquire) {
                        for session_id in &sessions {
                            // A close past checkpoint commit must finish its teardown.
                            if let Err(error) = self.cancel_lifecycle(session_id) {
                                tracing::debug!(%session_id, %error, "workspace close cancellation cannot interrupt this session");
                            }
                        }
                    }
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "Workspace retained; some sessions may already be stopped. Retry to close remaining sessions: {}",
            failures.join("; ")
        );
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Workspace close cancelled; workspace and drafts retained. Completed stops were not undone."
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
                crate::database::close_workspace(&workspace_id)
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
            ("history", "a", SessionState::Stopped),
            ("other", "b", SessionState::Running),
        ] {
            let session = super::super::tests::runtime_test_session(id, workspace, status);
            state.sessions.insert(id.into(), session);
        }
        let child = super::super::tests::runtime_test_subagent("child", "parent");
        state.subagents.insert("child".into(), child);
        assert_eq!(close_roots(&state, "a"), ["independent", "parent"]);
        state.sessions.get_mut("parent").unwrap().state = SessionState::Stopped;
        assert_eq!(close_roots(&state, "a"), ["child", "independent"]);
    }
}
