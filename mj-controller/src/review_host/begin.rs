use super::*;

impl HostState {
    /// Records one session's view for reviewer-child questions and recovery.
    pub(super) async fn observe(
        &mut self,
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
    ) {
        if snapshot.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.execution,
                mj_core::state::MaterializedExecutionState::Closing
                    | mj_core::state::MaterializedExecutionState::Closed
            )
        }) && self.reviews.contains_key(&session_id)
            && let Err(error) = self.resolve(&session_id, Resolution::Cancelled)
        {
            tracing::debug!(session_id, %error, "review was already committing when the session closed");
        }
        self.sessions.insert(
            session_id.clone(),
            SessionWatch {
                materialized: snapshot,
            },
        );
        let parents: Vec<_> = self
            .reviews
            .iter()
            .filter(|(_, slot)| {
                slot.state.active_group.as_ref().is_some_and(|group| {
                    group
                        .registrations
                        .iter()
                        .any(|item| item.child_session_id.as_deref() == Some(&session_id))
                })
            })
            .map(|(parent, _)| parent.clone())
            .collect();
        for parent in parents {
            self.publish(&parent);
        }
    }

    pub(super) fn begin_push(
        &mut self,
        session_id: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        let refusal = if crate::controller::move_session::move_owns_session(&session_id) {
            Some("session is moving")
        } else if self.reviews.contains_key(&session_id) || self.preparing.contains_key(&session_id)
        {
            Some(
                "a review is already open; wait for its verdict or cancel it before starting another",
            )
        } else {
            None
        };
        if let Some(reason) = refusal {
            let _ = reply.send(Err(StartRefusal(reason.into())));
            return;
        }
        let config = (self.config)(&session_id);
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        self.preparing.insert(
            session_id.clone(),
            PreparingOperation::Review {
                cancel: cancel.clone(),
            },
        );
        self.publish(&session_id);
        tokio::spawn(async move {
            // No deadline covers preparation as a whole: most of it is waiting
            // on the session actor, the worker or other work holding the
            // session, and each step that does work carries its own bound
            // (#1503). A cancellation drops whichever step is running.
            let started = std::time::Instant::now();
            let stages = PreparationStages::default();
            let prepared = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(StartRefusal(stages.cancelled())),
                prepared = prepare(
                    &control,
                    &environment,
                    &session_id,
                    config,
                    &stages,
                    path.map(|path| vec![std::path::PathBuf::from(path)]),
                ) => prepared,
            };
            tracing::info!(
                %session_id,
                elapsed_ms = started.elapsed().as_millis(),
                outcome = match &prepared {
                    Ok(_) => "prepared",
                    Err(refusal) => refusal.0.as_str(),
                },
                "review preparation finished"
            );
            let accepted = prepared.as_ref().ok().map(|prepared| {
                let repositories: Vec<_> = prepared.captured.iter().map(|delta| serde_json::json!({
                    "repository": delta.root, "base_commit": delta.base_commit, "base_kind": delta.base_kind,
                    "head_commit": delta.head_commit, "changed_file_count": delta.files.len()
                })).collect();
                serde_json::json!({"accepted": true, "repositories": repositories})
            });
            let (opened, result) = oneshot::channel();
            if events
                .send(HostEvent::Prepared {
                    session_id,
                    reply: Some(opened),
                    prepared: Box::new(prepared),
                })
                .is_err()
            {
                let _ = reply.send(Err(StartRefusal("the review host stopped".into())));
                return;
            }
            let outcome = result
                .await
                .unwrap_or_else(|_| Err(StartRefusal("the review host stopped".into())));
            let _ = reply.send(outcome.map(|()| accepted.expect("accepted review has a capture")));
        });
    }
}
