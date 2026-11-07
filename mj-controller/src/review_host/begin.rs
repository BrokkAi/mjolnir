use super::*;

impl HostState {
    /// Records one session's view for reviewer-child questions and recovery.
    pub(super) async fn observe(
        &mut self,
        session_id: String,
        snapshot: Option<Box<MaterializedSession>>,
    ) {
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
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.preparing.insert(
            session_id.clone(),
            PreparingOperation::Review {
                cancelled: cancelled.clone(),
            },
        );
        self.publish(&session_id);
        tokio::spawn(async move {
            let prepared = tokio::time::timeout(
                std::time::Duration::from_secs(90),
                prepare(
                    &control,
                    &environment,
                    &session_id,
                    config,
                    cancelled.clone(),
                    path.map(|path| vec![std::path::PathBuf::from(path)]),
                ),
            )
            .await;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(_) => {
                    cancelled.store(true, std::sync::atomic::Ordering::Release);
                    Err(StartRefusal(
                        "review preparation exceeded 90 seconds; call request_review again".into(),
                    ))
                }
            };
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
            let _ = reply.send(outcome.map(|note| {
                let mut accepted = accepted.expect("accepted review has a capture");
                if let (Some(note), Some(fields)) = (note, accepted.as_object_mut()) {
                    fields.insert("notice".into(), note.into());
                }
                accepted
            }));
        });
    }
}
