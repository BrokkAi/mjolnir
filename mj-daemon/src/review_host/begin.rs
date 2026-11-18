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
        let refusal = if mj_controller::controller::move_session::move_owns_session(&session_id) {
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
        if !config.is_push() {
            // Review is off for this session: a request is accepted without
            // choosing reviewers, and the pushed trees are recorded so a gate
            // installed while review was on lets the push through.
            self.spawn_operation(
                session_id,
                reply,
                "review-off",
                move |control, _persistence, _events, owner| run_review_off(control, owner, path),
            );
            return;
        }
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

/// Accepts a review request from a session whose review is off. It performs no
/// review; it only records the current trees as allowed to push.
async fn run_review_off(
    control: SessionManagerControl,
    session_id: String,
    path: Option<String>,
) -> Result<serde_json::Value, StartRefusal> {
    let protocol = control
        .session(session_id.clone())
        .await
        .map_err(|error| {
            StartRefusal(format!(
                "{error:#}; resume the session before requesting review"
            ))
        })?
        .view()
        .snapshot
        .and_then(|snapshot| snapshot.operational.relay_protocol_version)
        .unwrap_or(0);
    let repositories = path
        .map(|path| vec![std::path::PathBuf::from(path)])
        .filter(|_| protocol >= mj_core::relay::RELAY_WORKTREE_REVIEW_PROTOCOL);
    let outcome = resolve::push_action(
        &control,
        &session_id,
        ReviewerAction::CaptureDelta { repositories },
    )
    .await
    .map_err(|error| StartRefusal(format!("{error:#}")))?;
    let ReviewerOutcome::Delta { repositories } = outcome else {
        return Err(StartRefusal("the worker did not return a capture".into()));
    };
    let trees: std::collections::BTreeMap<_, _> = repositories
        .iter()
        .filter_map(|delta| {
            Some((
                delta.root.clone(),
                (delta.current_tree.clone(), delta.head_commit.clone()?),
            ))
        })
        .collect();
    if !trees.is_empty() {
        // A worker that predates overridden records can only write the
        // reviewed ref; with review off that moves no base anyone reads.
        let action = if protocol < mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL {
            ReviewerAction::RecordReviewed { trees }
        } else {
            ReviewerAction::RecordOverridden { trees }
        };
        resolve::push_action(&control, &session_id, action)
            .await
            .map_err(|error| StartRefusal(format!("{error:#}")))?;
    }
    Ok(serde_json::json!({
        "accepted": true,
        "review": "off",
        "message": "Review is off for this session: no reviewers ran. You may push.",
    }))
}
