use super::*;

impl HostState {
    /// Records one session's view for reviewer-child questions and recovery.
    pub async fn observe(
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
        }) && self.has_slot(&session_id)
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
        let parents: BTreeSet<_> = self
            .reviews
            .iter()
            .filter(|(_, slot)| {
                slot.open.active_group.as_ref().is_some_and(|group| {
                    group
                        .registrations
                        .iter()
                        .any(|item| item.child_session_id.as_deref() == Some(&session_id))
                })
            })
            .map(|(key, _)| key.session_id.clone())
            .collect();
        for parent in parents {
            self.publish(&parent);
        }
    }

    pub fn begin_push(
        &mut self,
        session_id: String,
        scope: super::reviewer::ReviewScope,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        if mj_session_runtime::session_manager::move_ownership::move_owns_session(&session_id) {
            let _ = reply.send(Err(StartRefusal("session is moving".into())));
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
                move |control, link, owner| run_review_off(control, link, owner, scope),
            );
            return;
        }
        // Whether this review may open next to the session's others is decided
        // when it is ready to open, from the roots its capture returned.
        self.next_epoch = self.next_epoch.saturating_add(1);
        let request = self.next_epoch;
        let cancel = tokio_util::sync::CancellationToken::new();
        let base_generation = self.base_generation(&session_id);
        self.preparations.insert(
            request,
            Preparation {
                session_id: session_id.clone(),
                scope: scope.clone(),
                cancel: cancel.clone(),
                base_generation,
                reply,
            },
        );
        self.publish(&session_id);
        self.spawn_preparation(request, session_id, scope, cancel, config);
    }

    /// Prepares the review that `request` asked for: captures its change and
    /// chooses its reviewers, then tells the host, which decides whether it
    /// may open.
    pub fn spawn_preparation(
        &self,
        request: u64,
        session_id: String,
        scope: super::reviewer::ReviewScope,
        cancel: tokio_util::sync::CancellationToken,
        config: ReviewConfig,
    ) {
        let control = self.control.clone();
        let environment = self.environment.clone();
        let events = self.events.clone();
        mj_core::runtime::spawn(async move {
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
                    scope,
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
            let _ = events.send(HostEvent::Prepared {
                request,
                prepared: Box::new(prepared),
            });
        });
    }
}

/// What the caller of an accepted review is told: the repositories it covers.
pub fn accepted_review(captured: &[mj_core::relay::RepoDelta]) -> serde_json::Value {
    let repositories: Vec<_> = captured
        .iter()
        .map(|delta| {
            serde_json::json!({
                "repository": delta.root, "base_commit": delta.base_commit, "base_kind": delta.base_kind,
                "head_commit": delta.head_commit, "changed_file_count": delta.files.len()
            })
        })
        .collect();
    serde_json::json!({"accepted": true, "repositories": repositories})
}

/// Accepts a review request from a session whose review is off. It performs no
/// review; it only records the current trees as allowed to push, after the host
/// agrees that no open review owns them.
async fn run_review_off(
    control: SessionManagerControl,
    link: OperationLink,
    session_id: String,
    scope: super::reviewer::ReviewScope,
) -> Result<serde_json::Value, StartRefusal> {
    let protocol = session_protocol(&control, &session_id).await?;
    let repositories = super::reviewer::capture_scope(scope, protocol)?;
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
        link.ask(OperationRequest::ReserveCaptured {
            protocol,
            roots: trees.keys().cloned().collect(),
        })
        .await?;
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
