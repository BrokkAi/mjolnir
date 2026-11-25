use super::*;

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
    pub fn override_push(
        &mut self,
        session_id: String,
        path: Option<String>,
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
        self.spawn_operation(session_id, reply, "override", |control, link, owner| {
            run_override(control, link, owner, path, overrides)
        });
    }
}

async fn run_override(
    control: SessionManagerControl,
    link: OperationLink,
    session_id: String,
    path: Option<String>,
    overrides: Vec<ReviewOverride>,
) -> Result<serde_json::Value, StartRefusal> {
    let protocol = session_protocol(&control, &session_id).await?;
    // An explicit path is resolved to its root before the host selects the
    // verdict; without one the host selects the only outstanding verdict.
    let roots = match path {
        Some(path) if concurrent_reviews(protocol) => {
            Some(resolve_roots(&control, &session_id, vec![std::path::PathBuf::from(path)]).await?)
        }
        Some(_) => {
            return Err(StartRefusal(
                "this session's worker is older than override_review's path argument; restart the session, or call override_review without a path".into(),
            ));
        }
        None => None,
    };
    let OperationAnswer::Reserved {
        findings: Some(findings),
    } = link
        .ask(OperationRequest::ReserveOverride { protocol, roots })
        .await?
    else {
        return Err(StartRefusal(
            "the review host did not select a findings verdict".into(),
        ));
    };
    let deltas = async {
        let repositories = (protocol >= mj_core::relay::RELAY_WORKTREE_REVIEW_PROTOCOL)
            .then(|| findings.trees.keys().cloned().collect());
        let outcome = resolve::push_action(
            &control,
            &session_id,
            ReviewerAction::CaptureDelta { repositories },
        )
        .await
        .map_err(|error| StartRefusal(format!("{error:#}")))?;
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
        Ok(deltas)
    }
    .await?;

    let declined = overrides
        .iter()
        // Continuation lines cannot impersonate a field or section boundary.
        .map(|entry| {
            format!(
                "- Finding: {}\n  Reason: {}",
                entry.finding.replace('\n', "\n    "),
                entry.reason.replace('\n', "\n    "),
            )
        })
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
    handle
        .submit(command_id, RelayCommand::RecordNotice { text: notice })
        .await
        .map_err(|error| StartRefusal(format!("could not record override notice: {error:#}")))?;

    // Keep pending findings until the worker ref is recorded. A retry can
    // safely repeat either write after a crash.
    link.ask_saved(OperationRequest::DeclineFindings { overrides })
        .await?;

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
    .map_err(|error| StartRefusal(format!("{error:#}")))?;
    link.ask_saved(OperationRequest::FinishOverride).await?;
    let repositories: Vec<_> = findings.trees.iter().map(|(root, (tree, head_commit))| serde_json::json!({ "repository": root, "tree": tree, "head_commit": head_commit })).collect();
    Ok(serde_json::json!({
        "accepted": true,
        "review_id": findings.review_id,
        "repositories": repositories,
        "message": if base_moved {
            "The findings are recorded as declined. You may push these commits. This session's worker is older than this feature, so the review base moved to these commits: later reviews cover only later changes. Restart the session to get the new behaviour."
        } else {
            "The findings are recorded as declined. You may push these commits. The review base is unchanged: later reviews still cover this change and are told not to report the declined findings again. Call set_baseline (named set_review_baseline in sessions started before it was renamed) with a reviewed commit to move the base."
        },
    }))
}
