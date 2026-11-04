use super::*;
use tokio_util::task::AbortOnDropHandle;

struct Read {
    generation: u64,
    task: Option<AbortOnDropHandle<()>>,
}

/// Owns summary requests for the current view. Leaving it cancels queued reads;
/// generations reject answers already delivered by an earlier visit.
pub(super) struct SummaryReads {
    reads: BTreeMap<String, Read>,
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
        updates: &UnboundedSender<DashboardIoUpdate>,
    ) -> Vec<String> {
        let removed = self
            .reads
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in &removed {
            self.reads.remove(id);
        }
        for (id, viewed) in wanted {
            if self.reads.contains_key(&id) {
                continue;
            }
            self.generation += 1;
            let task = spawn_stored_session_summary(
                id.clone(),
                viewed,
                self.generation,
                Arc::clone(&self.permits),
                updates.clone(),
            );
            self.reads.insert(
                id,
                Read {
                    generation: self.generation,
                    task: Some(AbortOnDropHandle::new(task)),
                },
            );
        }
        removed
    }

    pub(super) fn finish(&mut self, id: &str, generation: u64) -> bool {
        let Some(read) = self.reads.get_mut(id) else {
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
        reads.reconcile(wanted, &updates);
        let old = reads.reads["child-0"].generation;
        tokio::task::yield_now().await;
        assert_eq!(reads.reconcile(BTreeMap::new(), &updates).len(), 1200);
        reads.reconcile(BTreeMap::from([("child-0".into(), 0)]), &updates);
        let current = reads.reads["child-0"].generation;
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
