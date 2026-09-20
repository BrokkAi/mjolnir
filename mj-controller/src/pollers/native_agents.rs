//! Native transcript loading never delays the shared runtime subscription.
use super::*;
use mj_core::native_agent::{NativeAgentHistoryPage, NativeAgentSummary, NativeAgentView};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeFeedHealth {
    pub refresh_error: Option<String>,
    pub native_error: Option<String>,
}

impl RuntimeFeedHealth {
    pub fn error(&self) -> Option<String> {
        self.refresh_error
            .clone()
            .or_else(|| self.native_error.clone())
    }
}

#[derive(Default)]
pub(super) struct NativeAgentLoader {
    desired: BTreeMap<String, NativeAgentSummary>,
    views: BTreeMap<String, NativeAgentView>,
    failures: BTreeMap<String, (NativeAgentSummary, String)>,
    retry: BTreeSet<String>,
    active: BTreeSet<String>,
    tasks: tokio::task::JoinSet<(String, NativeAgentSummary, Result<NativeAgentView>)>,
}

impl NativeAgentLoader {
    pub fn update(&mut self, summaries: Vec<NativeAgentSummary>) {
        self.desired = summaries
            .into_iter()
            .map(|summary| (summary.agent.view_id(), summary))
            .collect();
        self.views.retain(|id, view| {
            self.desired
                .get(id)
                .is_some_and(|summary| summary.generation_ordinal == view.generation_ordinal)
        });
        // A subsequent successful poll gives transient read failures another try.
        self.failures
            .retain(|id, (summary, _)| self.desired.get(id) == Some(summary));
        self.retry = self.failures.keys().cloned().collect();
    }

    pub fn views(&self) -> Vec<NativeAgentView> {
        self.views.values().cloned().collect()
    }

    pub fn error(&self) -> Option<String> {
        self.failures
            .values()
            .next()
            .map(|(_, error)| error.clone())
    }

    fn pending(&self) -> Option<(String, NativeAgentSummary)> {
        self.desired
            .iter()
            .find(|(id, summary)| {
                !self.active.contains(*id)
                    && (!self.failures.contains_key(*id) || self.retry.contains(*id))
                    && self
                        .views
                        .get(*id)
                        .is_none_or(|view| !summary.is_satisfied_by(view))
            })
            .map(|(id, summary)| (id.clone(), summary.clone()))
    }

    pub fn has_work(&self) -> bool {
        !self.tasks.is_empty() || self.pending().is_some()
    }

    pub async fn next(&mut self) {
        self.next_with(load_native_projection).await;
    }

    async fn next_with<L, F>(&mut self, load: L)
    where
        L: Fn(NativeAgentSummary) -> F + Clone + Send + 'static,
        F: Future<Output = Result<NativeAgentView>> + Send + 'static,
    {
        while self.tasks.len() < 4 {
            let Some((id, summary)) = self.pending() else {
                break;
            };
            self.active.insert(id.clone());
            self.retry.remove(&id);
            let load = load.clone();
            self.tasks.spawn(async move {
                let result = load(summary.clone()).await;
                (id, summary, result)
            });
        }
        let Some(completed) = self.tasks.join_next().await else {
            return;
        };
        let (id, requested, result) = match completed {
            Ok(completed) => completed,
            Err(error) => {
                // A panic is reported for every affected read and retried on the next poll.
                tracing::warn!(%error, "native projection task failed");
                self.tasks = tokio::task::JoinSet::new();
                for id in std::mem::take(&mut self.active) {
                    if let Some(summary) = self.desired.get(&id) {
                        self.failures.insert(
                            id,
                            (
                                summary.clone(),
                                format!("Native agent reader failed: {error}"),
                            ),
                        );
                    }
                }
                return;
            }
        };
        self.active.remove(&id);
        let Some(desired) = self.desired.get(&id) else {
            return;
        };
        // Old work must never resurrect a deleted child or an earlier replay.
        if desired.generation_ordinal != requested.generation_ordinal {
            return;
        }
        match result {
            Ok(view) if desired.is_satisfied_by(&view) => {
                self.failures.remove(&id);
                self.views.insert(id, view);
            }
            Ok(_) => {} // The next read follows the newer published fingerprint.
            Err(error) if desired == &requested => {
                let message = format!(
                    "Could not refresh native agent {}: {error:#}",
                    desired.agent.name
                );
                tracing::warn!(%id, %message);
                self.failures.insert(id, (requested, message));
            }
            Err(error) => {
                tracing::debug!(%id, %error, "newer native publication superseded a failed read");
            }
        }
    }
}

async fn load_native_projection(summary: NativeAgentSummary) -> Result<NativeAgentView> {
    converge_native_projection(&summary, || {
        let owner = summary.agent.owner_session_id.clone();
        let child = summary.agent.session_id.clone();
        native_read(move || crate::database::load_native_agent_view(&owner, &child, 200))
    })
    .await
}

async fn converge_native_projection<F, Fut>(
    summary: &NativeAgentSummary,
    mut read: F,
) -> Result<NativeAgentView>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<NativeAgentView>>>,
{
    for attempt in 0..=runtime_feed::PROJECTION_CONVERGENCE_RETRIES {
        let view = read().await?;
        if let Some(view) = view
            && summary.is_satisfied_by(&view)
        {
            return Ok(view);
        }
        if attempt < runtime_feed::PROJECTION_CONVERGENCE_RETRIES {
            tokio::time::sleep(runtime_feed::PROJECTION_CONVERGENCE_RETRY_DELAY).await;
        }
    }
    bail!(
        "stored projection did not converge to generation {} at event {}",
        summary.generation_ordinal,
        summary.projection_ordinal
    )
}

async fn native_read<T: Send + 'static>(
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = Arc::clone(&runtime_feed::PROJECTION_READERS)
        .acquire_owned()
        .await
        .context("native projection readers stopped")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = read();
        if let Err(error) = &result {
            tracing::warn!(%error, "native projection read failed");
        }
        result
    })
    .await
    .context("native projection reader panicked")?
}

pub async fn load_native_agent_history(
    owner: String,
    child: String,
    before: Option<(u64, String)>,
) -> Result<NativeAgentHistoryPage> {
    native_read(move || crate::database::native_agent_history(&owner, &child, before)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::native_agent::{NativeAgent, NativeAgentCapabilities, NativeAgentState};
    use std::sync::atomic::AtomicUsize;

    fn view(child: &str, generation: u64, ordinal: u64) -> NativeAgentView {
        let agent = NativeAgent {
            owner_session_id: "owner".into(),
            session_id: child.into(),
            parent_session_id: None,
            name: child.into(),
            task: "Inspect".into(),
            capabilities: NativeAgentCapabilities::default(),
            state: NativeAgentState::Running,
        };
        let mut projection = MaterializedSession::empty(agent.view_id());
        projection.applied_event_ordinal = ordinal;
        projection.applied_event_digest = format!("digest-{ordinal}");
        NativeAgentView {
            generation_ordinal: generation,
            agent,
            projection,
        }
    }

    #[tokio::test]
    async fn unchanged_views_are_cached_and_changed_or_replayed_views_reload() {
        let mut loader = NativeAgentLoader::default();
        let original = view("child", 1, 2);
        loader.update(vec![NativeAgentSummary::of(&original)]);
        let answer = original.clone();
        loader
            .next_with(move |_| {
                let answer = answer.clone();
                async { Ok(answer) }
            })
            .await;
        assert_eq!(loader.views(), vec![original.clone()]);
        loader.update(vec![NativeAgentSummary::of(&original)]);
        assert!(!loader.has_work());

        let changed = view("child", 1, 3);
        loader.update(vec![NativeAgentSummary::of(&changed)]);
        let answer = changed.clone();
        loader
            .next_with(move |_| {
                let answer = answer.clone();
                async { Ok(answer) }
            })
            .await;
        assert_eq!(loader.views(), vec![changed]);

        let replayed = view("child", 4, 5);
        loader.update(vec![NativeAgentSummary::of(&replayed)]);
        assert!(loader.views().is_empty());
        let answer = replayed.clone();
        loader
            .next_with(move |_| {
                let answer = answer.clone();
                async { Ok(answer) }
            })
            .await;
        assert_eq!(loader.views(), vec![replayed]);
        loader.update(Vec::new());
        assert!(loader.views().is_empty());
        assert!(!loader.has_work());
    }

    #[tokio::test]
    async fn failed_reads_keep_last_good_view_and_recover_without_hiding_the_error_early() {
        let mut loader = NativeAgentLoader::default();
        let original = view("child", 1, 2);
        loader.update(vec![NativeAgentSummary::of(&original)]);
        let answer = original.clone();
        loader
            .next_with(move |_| {
                let answer = answer.clone();
                async { Ok(answer) }
            })
            .await;
        let changed = view("child", 1, 3);
        loader.update(vec![NativeAgentSummary::of(&changed)]);
        loader.next_with(|_| async { bail!("read failed") }).await;
        assert_eq!(loader.views(), vec![original]);
        assert!(loader.error().unwrap().contains("read failed"));
        assert!(!loader.has_work());
        loader.update(vec![NativeAgentSummary::of(&changed)]);
        assert!(loader.error().is_some());
        let answer = changed.clone();
        loader
            .next_with(move |_| {
                let answer = answer.clone();
                async { Ok(answer) }
            })
            .await;
        assert!(loader.error().is_none());
        assert_eq!(loader.views(), vec![changed]);
    }

    #[tokio::test]
    async fn slow_reads_are_bounded_and_cannot_resurrect_removed_children() {
        let mut loader = NativeAgentLoader::default();
        loader.update(
            (0..6)
                .map(|id| NativeAgentSummary::of(&view(&id.to_string(), 1, 2)))
                .collect(),
        );
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let load = {
            let started = started.clone();
            let release = release.clone();
            move |summary: NativeAgentSummary| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    release.acquire().await.unwrap().forget();
                    Ok(view(&summary.agent.session_id, 1, 2))
                }
            }
        };
        // Cancelling the select arm does not cancel or duplicate its owned readers.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), loader.next_with(load.clone()))
                .await
                .is_err()
        );
        assert_eq!(started.load(Ordering::SeqCst), 4);
        loader.update(Vec::new());
        release.add_permits(4);
        while loader.has_work() {
            loader.next_with(load.clone()).await;
        }
        assert!(loader.views().is_empty());
        assert_eq!(started.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_slow_child_does_not_delay_a_ready_sibling() {
        let mut loader = NativeAgentLoader::default();
        loader.update(vec![
            NativeAgentSummary::of(&view("slow", 1, 2)),
            NativeAgentSummary::of(&view("ready", 1, 2)),
        ]);
        tokio::time::timeout(
            Duration::from_secs(1),
            loader.next_with(|summary| async move {
                if summary.agent.session_id == "slow" {
                    std::future::pending::<()>().await;
                }
                Ok(view(&summary.agent.session_id, 1, 2))
            }),
        )
        .await
        .unwrap();
        assert_eq!(loader.views()[0].agent.session_id, "ready");
    }

    #[tokio::test]
    async fn read_panics_are_reported_and_retryable() {
        let mut loader = NativeAgentLoader::default();
        let original = view("child", 1, 2);
        loader.update(vec![NativeAgentSummary::of(&original)]);
        loader
            .next_with(|_| async {
                panic!("reader panic");
                #[allow(unreachable_code)]
                Ok(view("child", 1, 2))
            })
            .await;
        assert!(loader.error().unwrap().contains("reader panic"));
        assert!(!loader.has_work());
        loader.update(vec![NativeAgentSummary::of(&original)]);
        loader
            .next_with(|_| async { Ok(view("child", 1, 2)) })
            .await;
        assert!(loader.error().is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn lagging_projection_retries_but_persistent_mismatch_is_reported() {
        let expected = NativeAgentSummary::of(&view("child", 1, 3));
        let mut reads = 0;
        let result = converge_native_projection(&expected, || {
            reads += 1;
            let ordinal = if reads == 1 { 2 } else { 3 };
            async move { Ok(Some(view("child", 1, ordinal))) }
        })
        .await
        .unwrap();
        assert_eq!(result.projection.applied_event_ordinal, 3);
        assert_eq!(reads, 2);
        let error =
            converge_native_projection(&expected, || async { Ok(Some(view("child", 1, 2))) })
                .await
                .unwrap_err();
        assert!(error.to_string().contains("did not converge"), "{error}");
        let newer =
            converge_native_projection(&expected, || async { Ok(Some(view("child", 1, 4))) })
                .await
                .unwrap();
        assert_eq!(newer.projection.applied_event_ordinal, 4);
    }

    #[tokio::test]
    async fn replay_during_an_inflight_read_discards_the_old_generation() {
        let mut loader = NativeAgentLoader::default();
        loader.update(vec![NativeAgentSummary::of(&view("child", 1, 2))]);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let load = {
            let release = release.clone();
            move |_: NativeAgentSummary| {
                let release = release.clone();
                async move {
                    release.acquire().await.unwrap().forget();
                    Ok(view("child", 1, 2))
                }
            }
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(20), loader.next_with(load.clone()))
                .await
                .is_err()
        );
        loader.update(vec![NativeAgentSummary::of(&view("child", 4, 5))]);
        release.add_permits(1);
        loader.next_with(load).await;
        assert!(loader.views().is_empty());
        loader
            .next_with(|_| async { Ok(view("child", 4, 5)) })
            .await;
        assert_eq!(loader.views()[0].generation_ordinal, 4);
    }
}
