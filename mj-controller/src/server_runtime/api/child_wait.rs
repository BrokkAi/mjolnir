//! Child waits observe their owned inputs; unrelated publications do no work.
use super::*;
use tokio::sync::watch;

type Durable = watch::Receiver<Result<Arc<crate::database::CommittedState>, Arc<str>>>;
type Inputs = Vec<(Option<u64>, bool)>;

pub(super) struct ChildWaitFeed {
    durable: Option<Durable>,
    runtime: Option<watch::Receiver<u64>>,
    parent: Option<SessionHandle>,
}

impl ChildWaitFeed {
    fn committed(&self) -> Result<Option<Arc<crate::database::CommittedState>>> {
        self.durable
            .as_ref()
            .map(|receiver| match &*receiver.borrow() {
                Ok(state) => Ok(state.clone()),
                Err(error) => Err(anyhow!(error.to_string())),
            })
            .transpose()
    }
    pub(super) async fn new(backend: &ApiBackend, parent: &str) -> Result<Self> {
        Ok(Self {
            durable: if crate::database::database_writer_installed() {
                Some(crate::database::subscribe_committed_state()?)
            } else {
                None
            },
            runtime: backend.exports.revisions(),
            parent: backend.session_handle(parent.to_owned()).await?,
        })
    }

    pub(super) fn inputs(&self, backend: &ApiBackend, ids: &[String]) -> Result<Inputs> {
        let committed = self.committed()?;
        Ok(ids
            .iter()
            .map(|id| {
                (
                    committed
                        .as_ref()
                        .map(|state| state.wait_revisions.get(id).copied().unwrap_or(0)),
                    backend.exports.close_is_requested(id),
                )
            })
            .collect())
    }

    pub(super) async fn wait(
        &mut self,
        backend: &ApiBackend,
        parent_id: &str,
        ids: &[String],
        observed: &Inputs,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let now = mj_core::clock::epoch_millis();
        let grace = self
            .committed()?
            .and_then(|state| {
                ids.iter()
                    .filter_map(|id| {
                        if !matches!(
                            child_progress(&state, id).report,
                            ReportState::Pending { .. }
                        ) {
                            return None;
                        }
                        let end = state
                            .subagent_reports
                            .get(id)?
                            .reminder
                            .as_ref()?
                            .sent_at_ms
                            .saturating_add(mj_core::subagent::HANDBACK_REMINDER_GRACE_MS);
                        (end > now).then_some(
                            tokio::time::Instant::now() + Duration::from_millis((end - now) as u64),
                        )
                    })
                    .min()
            })
            .unwrap_or(deadline)
            .min(deadline);
        loop {
            if self.inputs(backend, ids)? != *observed {
                return Ok(());
            }
            // Offline test backends have no publication owner. Their durable
            // observation remains an explicit poll, never a daemon fallback.
            let offline = self.durable.is_none() && self.runtime.is_none();
            tokio::select! {
                result = changed(&mut self.durable) => result?,
                result = changed(&mut self.runtime) => result?,
                _ = tokio::time::sleep_until(grace) => return Ok(()),
                _ = async {
                    match self.parent.as_mut() {
                        Some(parent) => { let _ = parent.changed().await; }
                        None => std::future::pending().await,
                    }
                } => {
                    if self.parent.as_ref().is_some_and(SessionHandle::is_stopped) {
                        self.parent = backend.session_handle(parent_id.to_owned()).await?;
                        if self.parent.as_ref().is_some_and(SessionHandle::is_stopped) {
                            self.parent = None;
                        }
                    }
                    return Ok(());
                },
                _ = async {
                    if offline { tokio::time::sleep(Duration::from_millis(250)).await; }
                    else { std::future::pending().await }
                } => return Ok(()),
            }
            if self.parent.is_none() {
                self.parent = backend.session_handle(parent_id.to_owned()).await?;
                if self.parent.is_some() {
                    return Ok(());
                }
            }
        }
    }
}

async fn changed<T>(receiver: &mut Option<watch::Receiver<T>>) -> Result<()> {
    match receiver {
        Some(receiver) => receiver
            .changed()
            .await
            .context("child observation feed stopped"),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoSessions;
    impl mj_client::session::SessionControlBackend for NoSessions {
        fn session(&self, _: String) -> BoxFuture<'_, Result<SessionHandle>> {
            Box::pin(async { bail!("no live actor") })
        }
    }
    struct NoExports;
    impl ExportRuntime for NoExports {
        fn session_record(&self, _: &str) -> Option<mj_core::state::SessionRecord> {
            None
        }
        fn checkpoint_now(
            &self,
            _: String,
        ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
            Box::pin(async { bail!("unused") })
        }
    }

    #[tokio::test]
    async fn unrelated_publications_do_not_end_child_wait_but_owned_changes_and_deadlines_do() {
        let backend = ApiBackend::new(
            SessionControl::new(NoSessions),
            Arc::new(|_| None),
            Arc::new(NoExports),
        );
        let committed = crate::database::CommittedState {
            sequence: 0,
            state: Default::default(),
            moves: Default::default(),
            native_agents: Default::default(),
            startup_groups: Default::default(),
            subagent_reports: Default::default(),
            turns: Default::default(),
            wait_revisions: Default::default(),
        };
        let (publication, receiver) = watch::channel(Ok(Arc::new(committed.clone())));
        let mut feed = ChildWaitFeed {
            durable: Some(receiver),
            runtime: None,
            parent: None,
        };
        let ids = vec!["child".into()];
        {
            let observed = feed.inputs(&backend, &ids).unwrap();
            let waiter = feed.wait(
                &backend,
                "parent",
                &ids,
                &observed,
                tokio::time::Instant::now() + Duration::from_secs(5),
            );
            tokio::pin!(waiter);
            let updates = async {
                for sequence in 1..=50 {
                    let mut next = committed.clone();
                    next.sequence = sequence;
                    next.wait_revisions.insert_shared("other".into(), sequence);
                    publication.send_replace(Ok(Arc::new(next))).unwrap();
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(350)).await;
            };
            tokio::select! {
                result = &mut waiter => panic!("unrelated publications or a polling tick ended the wait: {result:?}"),
                _ = updates => {},
            }
            let mut next = committed.clone();
            next.wait_revisions.insert_shared("child".into(), 51);
            publication.send_replace(Ok(Arc::new(next))).unwrap();
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap();
        }
        let observed = feed.inputs(&backend, &ids).unwrap();
        feed.wait(
            &backend,
            "parent",
            &ids,
            &observed,
            tokio::time::Instant::now(),
        )
        .await
        .unwrap();
    }
}
