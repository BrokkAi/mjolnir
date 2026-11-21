use super::*;

impl HostState {
    pub fn set_push_base(
        &mut self,
        session_id: String,
        commit: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
    ) {
        if let Some(refusal) = self.push_operation_refusal(&session_id) {
            let _ = reply.send(Err(StartRefusal(refusal.into())));
            return;
        }
        self.spawn_operation(
            session_id,
            reply,
            "baseline update",
            |control, link, owner| run_set_base(control, link, owner, commit, path),
        );
    }
}

async fn run_set_base(
    control: SessionManagerControl,
    link: OperationLink,
    session_id: String,
    commit: String,
    path: Option<String>,
) -> Result<serde_json::Value, StartRefusal> {
    let protocol = session_protocol(&control, &session_id).await?;
    if protocol < mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL {
        return Err(StartRefusal(
            "this session's worker is older than set_baseline; restart the session, then call it again"
                .into(),
        ));
    }
    // The worker names the repository the way it resolves it for the move, so
    // the roots the host reserves are the ones the move changes. A worker that
    // cannot resolve without a capture runs the move alone.
    let root = if concurrent_reviews(protocol) {
        let named = path
            .as_deref()
            .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
        resolve_roots(&control, &session_id, vec![named])
            .await?
            .into_iter()
            .next()
    } else {
        None
    };
    link.ask(OperationRequest::ReserveBase {
        protocol,
        root: root.clone(),
    })
    .await?;

    // The move names the root that was reserved, not the path the caller
    // gave: a path can resolve to another worktree between the two requests.
    let target = root.or_else(|| path.map(std::path::PathBuf::from));
    let outcome = resolve::push_action(
        &control,
        &session_id,
        ReviewerAction::SetReviewBase {
            path: target,
            commit,
        },
    )
    .await
    .map_err(|error| StartRefusal(format!("{error:#}")))?;
    let ReviewerOutcome::ReviewBaseSet {
        root,
        commit,
        tree,
        changed,
    } = outcome
    else {
        return Err(StartRefusal(
            "the worker did not confirm the review base".into(),
        ));
    };
    link.ask_saved(OperationRequest::FinishBase {
        root: root.clone(),
        changed,
    })
    .await?;
    if !changed {
        return Ok(
            serde_json::json!({"accepted": true, "repository": root, "commit": commit, "tree": tree, "message": format!("The review base already includes later reviewed commit {commit}; the base and declined findings are unchanged.")}),
        );
    }
    Ok(serde_json::json!({
        "accepted": true,
        "repository": root,
        "commit": commit,
        "tree": tree,
        "message": format!("Later reviews of {} start from commit {commit}. The declined-findings list is cleared.", root.display()),
    }))
}
