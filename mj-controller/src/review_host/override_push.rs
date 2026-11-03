use super::*;
use futures::FutureExt;

const OVERRIDE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

impl HostState {
    pub(super) fn override_push(
        &mut self,
        session_id: String,
        reason: String,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        let refusal = if !(self.config)(&session_id).is_push() {
            Some("this session's review trigger is not push")
        } else if crate::controller::move_session::move_owns_session(&session_id) {
            Some("session is moving")
        } else if self.reviews.contains_key(&session_id)
            || self.preparing.contains_key(&session_id)
            || self.recovery_candidates.contains(&session_id)
            || self.recovery_in_flight.contains(&session_id)
        {
            Some("a review is open; wait for its verdict or cancel it")
        } else if reason.trim().is_empty() {
            Some("override_review needs a reason")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            let _ = reply.send(Err(StartRefusal(refusal.into())));
            return;
        }
        let control = self.control.clone();
        let persistence = self
            .persistence
            .as_ref()
            .expect("review persistence is live")
            .clone();
        let events = self.events.clone();
        let owner = session_id.clone();
        let (completed, result) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_override(
                control,
                persistence,
                events.clone(),
                owner.clone(),
                reason,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                tracing::error!(session_id = %owner, "push review override task panicked");
                Err(StartRefusal("push review override task panicked".into()))
            });
            let _ = completed.send(result);
            let _ = events.send(HostEvent::OverrideFinished { session_id: owner });
        });
        self.preparing.insert(
            session_id.clone(),
            PreparingOperation::Override {
                task,
                admitted: false,
                result,
                reply,
            },
        );
        self.publish(&session_id);
    }

    pub(super) fn override_validated(
        &mut self,
        session_id: &str,
        permit: oneshot::Sender<Result<crate::upgrade::Work, StartRefusal>>,
    ) {
        let Some(PreparingOperation::Override { admitted, .. }) =
            self.preparing.get_mut(session_id)
        else {
            let _ = permit.send(Err(StartRefusal("the review host stopped".into())));
            return;
        };
        if crate::controller::move_session::move_owns_session(session_id) {
            let _ = permit.send(Err(StartRefusal("session is moving".into())));
            return;
        }
        let work =
            crate::upgrade::activity_unless_draining("push review override").map_err(|error| {
                StartRefusal(format!("push review override was not admitted: {error:#}"))
            });
        *admitted = work.is_ok();
        let _ = permit.send(work);
    }

    pub(super) fn override_finished(&mut self, session_id: &str) {
        if let Some(PreparingOperation::Override {
            mut result, reply, ..
        }) = self.preparing.remove(session_id)
        {
            let response = result
                .try_recv()
                .unwrap_or_else(|_| Err(StartRefusal("push review override task stopped".into())));
            let _ = reply.send(response);
            self.publish(session_id);
        }
    }
}

async fn run_override(
    control: SessionManagerControl,
    persistence: mpsc::UnboundedSender<PersistenceRequest>,
    events: mpsc::UnboundedSender<HostEvent>,
    session_id: String,
    reason: String,
) -> Result<serde_json::Value, StartRefusal> {
    let (mut state, findings, deltas) = tokio::time::timeout(OVERRIDE_TIMEOUT, async {
        let (reply, loaded) = oneshot::channel();
        persistence
            .send(PersistenceRequest::Load {
                session_id: session_id.clone(),
                reply,
            })
            .map_err(|_| StartRefusal("review persistence stopped".into()))?;
        let state = loaded
            .await
            .map_err(|_| StartRefusal("review persistence stopped".into()))?
            .map_err(StartRefusal)?;
        let findings = state.push_findings.clone().ok_or_else(|| {
            StartRefusal("no findings verdict is pending; call request_review".into())
        })?;
        let outcome = resolve::push_action(
            &control,
            &session_id,
            ReviewerAction::CaptureDelta {
                scope: mj_core::relay::CaptureScope::PushableCommits,
                baselines: state.baselines.clone(),
            },
        )
        .await
        .map_err(StartRefusal)?;
        let ReviewerOutcome::Delta {
            repositories: deltas,
        } = outcome
        else {
            return Err(StartRefusal(
                "the worker did not return a push review capture".into(),
            ));
        };
        let roots: BTreeSet<_> = deltas.iter().map(|delta| delta.root.clone()).collect();
        if deltas.is_empty()
            || roots.len() != deltas.len()
            || roots.len() != findings.trees.len()
            || deltas.iter().any(|delta| {
                findings
                    .trees
                    .get(&delta.root)
                    .is_none_or(|(tree, _)| tree != &delta.current_tree)
            })
        {
            return Err(StartRefusal(
                "HEAD changed since the last verdict; call request_review".into(),
            ));
        }
        Ok((state, findings, deltas))
    })
    .await
    .map_err(|_| StartRefusal("push review override preparation exceeded 90 seconds".into()))??;

    let (permit, admitted) = oneshot::channel();
    events
        .send(HostEvent::OverrideValidated {
            session_id: session_id.clone(),
            permit,
        })
        .map_err(|_| StartRefusal("the review host stopped".into()))?;
    let _admission = admitted
        .await
        .map_err(|_| StartRefusal("the review host stopped".into()))??;

    let reason = mj_review::bound_tail(reason.trim(), 4096, "override reason");
    let synthesis = mj_review::bound_tail(&findings.synthesis, 16 * 1024, "overridden findings");
    let mut notice = format!(
        "Before-push review override requested by the agent. Reason: {reason}\nOverridden findings (review {}):\n{synthesis}\nRepositories:\n",
        mj_review::bound_tail(&findings.review_id, 512, "review id")
    );
    let mut repositories = String::new();
    for delta in &deltas {
        repositories.push_str(&format!(
            "{}: {}..{} (tree {})\n",
            mj_review::bound_tail(&delta.root.to_string_lossy(), 1024, "repository path"),
            mj_review::bound_tail(
                delta.base_commit.as_deref().unwrap_or("?"),
                128,
                "base commit"
            ),
            mj_review::bound_tail(
                delta.head_commit.as_deref().unwrap_or("?"),
                128,
                "head commit"
            ),
            mj_review::bound_tail(&delta.current_tree, 128, "tree")
        ));
    }
    notice.push_str(&mj_review::bound_tail(
        &repositories,
        8 * 1024,
        "repositories",
    ));
    let handle = control
        .session(session_id.clone())
        .await
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
    let command_id = new_command_id("push-review-override-notice")
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        handle.submit(command_id, RelayCommand::RecordNotice { text: notice }),
    )
    .await
    .map_err(|_| {
        StartRefusal("the worker did not record the override notice within 10 seconds".into())
    })?
    .map_err(|error| StartRefusal(format!("could not record override notice: {error:#}")))?;

    resolve::push_action(
        &control,
        &session_id,
        ReviewerAction::RecordReviewed {
            trees: findings.trees.clone(),
        },
    )
    .await
    .map_err(StartRefusal)?;
    let trees: BTreeMap<_, _> = findings
        .trees
        .iter()
        .map(|(root, (tree, _))| (root.clone(), tree.clone()))
        .collect();
    if let Err(error) = resolve::push_action(
        &control,
        &session_id,
        ReviewerAction::AdvanceBaseline {
            trees: trees.clone(),
        },
    )
    .await
    {
        tracing::warn!(%session_id, %error, "could not pin the overridden push review baseline");
    }
    state.baselines = trees;
    state.prior_review = None;
    state.push_findings = None;
    let (reply, saved) = oneshot::channel();
    persistence
        .send(PersistenceRequest::SaveAcknowledged {
            session_id,
            state: Box::new(state),
            reply,
        })
        .map_err(|_| {
            StartRefusal("review persistence stopped after recording the reviewed tree".into())
        })?;
    saved
        .await
        .map_err(|_| {
            StartRefusal("review persistence stopped after recording the reviewed tree".into())
        })?
        .map_err(StartRefusal)?;
    let repositories: Vec<_> = findings.trees.iter().map(|(root, (tree, head_commit))| serde_json::json!({ "repository": root, "tree": tree, "head_commit": head_commit })).collect();
    Ok(
        serde_json::json!({ "accepted": true, "review_id": findings.review_id, "repositories": repositories }),
    )
}
