use super::*;
use tokio_util::task::AbortOnDropHandle;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ReadKey {
    Summary(String),
    Item(String, String),
    History(String, String),
}

struct Read {
    generation: u64,
    task: Option<AbortOnDropHandle<()>>,
}

/// Owns summary and located-message requests for the current view. Leaving it cancels queued reads;
/// generations reject answers already delivered by an earlier visit.
pub(super) struct SummaryReads {
    reads: BTreeMap<ReadKey, Read>,
    generation: u64,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for SummaryReads {
    fn default() -> Self {
        Self {
            reads: BTreeMap::new(),
            generation: 0,
            permits: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}

impl SummaryReads {
    pub(super) fn reconcile(
        &mut self,
        wanted: BTreeMap<String, u64>,
        item: Option<(String, String)>,
        navigation: BTreeMap<ReadKey, u64>,
        updates: &UnboundedSender<DashboardIoUpdate>,
    ) -> Vec<String> {
        let mut requests: BTreeMap<_, _> = wanted
            .into_iter()
            .map(|(id, viewed)| (ReadKey::Summary(id), viewed))
            .collect();
        if let Some((session_id, stable_id)) = item {
            requests.insert(ReadKey::Item(session_id, stable_id), 0);
        }
        requests.extend(navigation);
        let removed_keys = self
            .reads
            .keys()
            .filter(|key| !requests.contains_key(*key))
            .cloned()
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        for key in removed_keys {
            self.reads.remove(&key);
            if let ReadKey::Summary(id) = key {
                removed.push(id);
            }
        }
        for (key, viewed) in requests {
            if self.reads.contains_key(&key) {
                continue;
            }
            self.generation += 1;
            let task = match &key {
                ReadKey::Summary(id) => spawn_stored_session_summary(
                    id.clone(),
                    viewed,
                    self.generation,
                    Arc::clone(&self.permits),
                    updates.clone(),
                ),
                ReadKey::History(session_id, stable_id) => io::spawn_notification_history(
                    session_id.clone(),
                    stable_id.clone(),
                    usize::try_from(viewed).unwrap_or(usize::MAX),
                    self.generation,
                    Arc::clone(&self.permits),
                    updates.clone(),
                ),
                ReadKey::Item(session_id, stable_id) => io::spawn_stored_transcript_item(
                    session_id.clone(),
                    stable_id.clone(),
                    self.generation,
                    Arc::clone(&self.permits),
                    updates.clone(),
                ),
            };
            self.reads.insert(
                key,
                Read {
                    generation: self.generation,
                    task: Some(AbortOnDropHandle::new(task)),
                },
            );
        }
        removed
    }

    pub(super) fn loading_sessions(&self) -> impl Iterator<Item = &String> {
        self.reads
            .iter()
            .filter(|(_, read)| read.task.is_some())
            .filter_map(|(key, _)| match key {
                ReadKey::Summary(id) => Some(id),
                ReadKey::Item(..) | ReadKey::History(..) => None,
            })
    }

    pub(super) fn item_loading(&self, session_id: &str, stable_id: &str) -> bool {
        self.reads
            .get(&ReadKey::Item(session_id.to_owned(), stable_id.to_owned()))
            .is_some_and(|read| read.task.is_some())
    }

    pub(super) fn finish_history(
        &mut self,
        session_id: &str,
        stable_id: &str,
        generation: u64,
    ) -> bool {
        self.finish_key(
            &ReadKey::History(session_id.to_owned(), stable_id.to_owned()),
            generation,
        )
    }

    pub(super) fn finish_item(
        &mut self,
        session_id: &str,
        stable_id: &str,
        generation: u64,
    ) -> bool {
        self.finish_key(
            &ReadKey::Item(session_id.to_owned(), stable_id.to_owned()),
            generation,
        )
    }

    pub(super) fn finish(&mut self, id: &str, generation: u64) -> bool {
        self.finish_key(&ReadKey::Summary(id.to_owned()), generation)
    }

    fn finish_key(&mut self, key: &ReadKey, generation: u64) -> bool {
        let Some(read) = self.reads.get_mut(key) else {
            return false;
        };
        if read.generation != generation || read.task.is_none() {
            return false;
        }
        read.task = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn leaving_a_family_cancels_queued_reads_and_rejects_previous_visits() {
        let mut reads = SummaryReads::default();
        // Hold all network admission while a large family queues its reads.
        // Cancelled tasks must disappear without ever consulting a daemon.
        let held = reads.permits.clone().acquire_many_owned(4).await.unwrap();
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        let wanted = (0..1200).map(|id| (format!("child-{id}"), 0)).collect();
        let item = Some(("notification".into(), "outside-tail".into()));
        let navigation = BTreeMap::from([(
            ReadKey::History("notification".into(), "buried".into()),
            600,
        )]);
        reads.reconcile(wanted, item.clone(), navigation.clone(), &updates);
        let old_history =
            reads.reads[&ReadKey::History("notification".into(), "buried".into())].generation;
        let old_item =
            reads.reads[&ReadKey::Item("notification".into(), "outside-tail".into())].generation;
        let old = reads.reads[&ReadKey::Summary("child-0".into())].generation;
        tokio::task::yield_now().await;
        assert_eq!(
            reads
                .reconcile(BTreeMap::new(), None, BTreeMap::new(), &updates)
                .len(),
            1200
        );
        reads.reconcile(
            BTreeMap::from([("child-0".into(), 0)]),
            item,
            navigation,
            &updates,
        );
        let current_history =
            reads.reads[&ReadKey::History("notification".into(), "buried".into())].generation;
        assert!(!reads.finish_history("notification", "buried", old_history));
        assert!(reads.finish_history("notification", "buried", current_history));
        let current_item =
            reads.reads[&ReadKey::Item("notification".into(), "outside-tail".into())].generation;
        assert!(!reads.finish_item("notification", "outside-tail", old_item));
        assert!(reads.finish_item("notification", "outside-tail", current_item));
        let current = reads.reads[&ReadKey::Summary("child-0".into())].generation;
        assert!(
            !reads.finish("child-0", old),
            "a previous visit cannot finish the new read"
        );
        assert!(reads.finish("child-0", current));
        tokio::time::timeout(Duration::from_secs(2), async {
            // Only the owner and the held permit may retain the semaphore.
            while Arc::strong_count(&reads.permits) > 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("leaving a family must cancel its queued network work");
        assert!(matches!(
            results.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(held);
    }
}
