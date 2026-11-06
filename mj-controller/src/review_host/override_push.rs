use super::*;
use futures::FutureExt;

const OVERRIDE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// Most findings one `override_review` call may decline.
const MAX_OVERRIDES: usize = 32;
const FINDING_BYTES: usize = 2 * 1024;
const REASON_BYTES: usize = 4 * 1024;
/// The notice lists every override; keep the whole list bounded.
const NOTICE_OVERRIDES_BYTES: usize = 24 * 1024;

/// Trims and bounds an `override_review` request, or says how to fix it.
fn validated_overrides(
    overrides: Vec<ReviewOverride>,
) -> Result<Vec<ReviewOverride>, &'static str> {
    if overrides.is_empty() {
        return Err("override_review needs at least one finding with a reason");
    }
    if overrides.len() > MAX_OVERRIDES {
        return Err(
            "override_review takes at most 32 findings; group related findings into one entry",
        );
    }
    overrides
        .into_iter()
        .map(|entry| {
            let finding = entry.finding.trim();
            let reason = entry.reason.trim();
            if finding.is_empty() {
                Err("every override_review entry needs the finding it declines")
            } else if reason.is_empty() {
                Err("every override_review entry needs a reason")
            } else {
                Ok(ReviewOverride {
                    finding: mj_review::bound_tail(finding, FINDING_BYTES, "finding"),
                    reason: mj_review::bound_tail(reason, REASON_BYTES, "override reason"),
                })
            }
        })
        .collect()
}

impl HostState {
    pub(super) fn override_push(
        &mut self,
        session_id: String,
        overrides: Vec<ReviewOverride>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        let overrides = validated_overrides(overrides);
        let refusal = self
            .push_operation_refusal(&session_id)
            .or_else(|| overrides.as_ref().err().copied());
        if let Some(refusal) = refusal {
            let _ = reply.send(Err(StartRefusal(refusal.into())));
            return;
        }
        let Ok(overrides) = overrides else {
            return;
        };
        self.spawn_operation(
            session_id,
            reply,
            "override",
            |control, persistence, events, owner| {
                run_override(control, persistence, events, owner, overrides)
            },
        );
    }

    /// Why a push-review state change cannot start now, with how to proceed.
    pub(super) fn push_operation_refusal(&self, session_id: &str) -> Option<&'static str> {
        if !(self.config)(session_id).is_push() {
            Some("this session's review trigger is not push; this tool only works for push review")
        } else if crate::controller::move_session::move_owns_session(session_id) {
            Some("session is moving; try again after the move finishes")
        } else if self.reviews.contains_key(session_id)
            || self.preparing.contains_key(session_id)
            || self.recovery_candidates.contains(session_id)
            || self.recovery_in_flight.contains(session_id)
        {
            Some("a review is open; wait for its verdict or cancel it, then try again")
        } else {
            None
        }
    }

    /// Runs one push-review operation that changes recorded review state in a
    /// task, holding the session's `preparing` slot until it finishes.
    pub(super) fn spawn_operation<F>(
        &mut self,
        session_id: String,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
        name: &'static str,
        run: impl FnOnce(
            SessionManagerControl,
            mpsc::UnboundedSender<PersistenceRequest>,
            mpsc::UnboundedSender<HostEvent>,
            String,
        ) -> F,
    ) where
        F: std::future::Future<Output = Result<serde_json::Value, StartRefusal>> + Send + 'static,
    {
        let control = self.control.clone();
        let persistence = self
            .persistence
            .as_ref()
            .expect("review persistence is live")
            .clone();
        let events = self.events.clone();
        let owner = session_id.clone();
        let operation = run(control, persistence, events.clone(), owner.clone());
        let (completed, result) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(operation)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(session_id = %owner, "push review {name} task panicked");
                    Err(StartRefusal(format!("push review {name} task panicked")))
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
    overrides: Vec<ReviewOverride>,
) -> Result<serde_json::Value, StartRefusal> {
    let (mut state, findings, deltas, protocol) = tokio::time::timeout(OVERRIDE_TIMEOUT, async {
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
        let protocol = control
            .session(session_id.clone())
            .await
            .map_err(|error| StartRefusal(format!("{error:#}")))?
            .view()
            .snapshot
            .and_then(|snapshot| snapshot.operational.relay_protocol_version)
            .unwrap_or(0);
        let repositories = (protocol >= mj_core::relay::RELAY_WORKTREE_REVIEW_PROTOCOL)
            .then(|| findings.trees.keys().cloned().collect());
        let outcome = resolve::push_action(
            &control,
            &session_id,
            ReviewerAction::CaptureDelta {
                repositories,
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
        Ok((state, findings, deltas, protocol))
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

    let declined = overrides
        .iter()
        .map(|entry| format!("- Finding: {}\n  Reason: {}", entry.finding, entry.reason))
        .collect::<Vec<_>>()
        .join("\n");
    let declined = mj_review::bound_tail(&declined, NOTICE_OVERRIDES_BYTES, "overrides");
    let synthesis = mj_review::bound_tail(&findings.synthesis, 16 * 1024, "overridden findings");
    let mut notice = format!(
        "Before-push review override requested by the agent. The agent declined to fix these findings:\n{declined}\nOverridden synthesis (review {}):\n{synthesis}\nRepositories:\n",
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

    // A worker that predates overridden records can only write the reviewed
    // ref, which also moves the base of the next review.
    let base_moved = protocol < mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL;
    let trees = findings.trees.clone();
    resolve::push_action(
        &control,
        &session_id,
        if base_moved {
            ReviewerAction::RecordReviewed { trees }
        } else {
            ReviewerAction::RecordOverridden { trees }
        },
    )
    .await
    .map_err(StartRefusal)?;
    state.decline_findings(overrides);
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
    Ok(serde_json::json!({
        "accepted": true,
        "review_id": findings.review_id,
        "repositories": repositories,
        "message": if base_moved {
            "The findings are recorded as declined. You may push these commits. This session's worker is older than this feature, so the review base moved to these commits: later reviews cover only later changes. Restart the session to get the new behaviour."
        } else {
            "The findings are recorded as declined. You may push these commits. The review base is unchanged: later reviews still cover this change and are told not to report the declined findings again. Call set_review_baseline with a reviewed commit to move the base."
        },
    }))
}
