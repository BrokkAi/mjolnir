use super::*;

const SET_BASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

impl HostState {
    pub(super) fn set_push_base(
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
            |control, persistence, events, owner| {
                run_set_base(control, persistence, events, owner, commit, path)
            },
        );
    }
}

async fn run_set_base(
    control: SessionManagerControl,
    persistence: mpsc::UnboundedSender<PersistenceRequest>,
    events: mpsc::UnboundedSender<HostEvent>,
    session_id: String,
    commit: String,
    path: Option<String>,
) -> Result<serde_json::Value, StartRefusal> {
    let mut state = tokio::time::timeout(SET_BASE_TIMEOUT, async {
        let protocol = control
            .session(session_id.clone())
            .await
            .map_err(|error| StartRefusal(format!("{error:#}")))?
            .view()
            .snapshot
            .and_then(|snapshot| snapshot.operational.relay_protocol_version)
            .unwrap_or(0);
        if protocol < mj_core::relay::RELAY_REVIEW_BASE_PROTOCOL {
            return Err(StartRefusal(
                "this session's worker is older than set_review_baseline; restart the session, then call it again"
                    .into(),
            ));
        }
        let (reply, loaded) = oneshot::channel();
        persistence
            .send(PersistenceRequest::Load {
                session_id: session_id.clone(),
                reply,
            })
            .map_err(|_| StartRefusal("review persistence stopped".into()))?;
        loaded
            .await
            .map_err(|_| StartRefusal("review persistence stopped".into()))?
            .map_err(StartRefusal)
    })
    .await
    .map_err(|_| StartRefusal("set_review_baseline preparation exceeded 90 seconds".into()))??;

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

    let outcome = resolve::push_action(
        &control,
        &session_id,
        ReviewerAction::SetReviewBase {
            path: path.map(std::path::PathBuf::from),
            commit,
        },
    )
    .await
    .map_err(StartRefusal)?;
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
    if !changed {
        return Ok(
            serde_json::json!({"accepted": true, "repository": root, "commit": commit, "tree": tree, "message": format!("The review base already includes later reviewed commit {commit}; the base and declined findings are unchanged.")}),
        );
    }
    // Only this repository starts a new range; other declined ranges remain.
    state.advance_review_bases(std::slice::from_ref(&root));
    let (reply, saved) = oneshot::channel();
    persistence
        .send(PersistenceRequest::SaveAcknowledged {
            session_id,
            state: Box::new(state),
            reply,
        })
        .map_err(|_| {
            StartRefusal("review persistence stopped after the review base moved".into())
        })?;
    saved
        .await
        .map_err(|_| StartRefusal("review persistence stopped after the review base moved".into()))?
        .map_err(StartRefusal)?;
    Ok(serde_json::json!({
        "accepted": true,
        "repository": root,
        "commit": commit,
        "tree": tree,
        "message": format!("Later reviews of {} start from commit {commit}. The declined-findings list is cleared.", root.display()),
    }))
}
