//! Per-connection replication. The durable queue remains the only work owner.
use super::*;

#[derive(Default)]
pub(in crate::worker_runtime) struct SubagentFeed {
    known: BTreeMap<String, SubagentQueueChange>,
    pending: Option<SubagentQueuePage>,
    sequence: u64,
    pending_generation: Option<u64>,
    known_generation: Option<u64>,
}

impl SubagentFeed {
    pub fn poll(
        &mut self,
        endpoint: Option<&SubagentEndpoint>,
        acknowledged: Option<u64>,
    ) -> Result<SubagentQueuePage> {
        if let Some(page) = &self.pending {
            if acknowledged != Some(page.sequence) {
                return Ok(page.clone());
            }
            let receipts = page
                .changes
                .iter()
                .filter_map(|change| match change {
                    SubagentQueueChange::Completed(receipt) => Some(receipt.request_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if let Some(endpoint) = endpoint {
                endpoint.acknowledge_replies(&receipts, false)?;
            }
            for change in &page.changes {
                match change {
                    SubagentQueueChange::Retired { request_id } => {
                        self.known.remove(request_id);
                    }
                    _ => {
                        self.known
                            .insert(change.request_id().to_owned(), change.clone());
                    }
                }
            }
            if !page.has_more {
                self.known_generation = self.pending_generation;
            }
            self.pending = None;
        }
        let mut current = BTreeMap::new();
        self.pending_generation = None;
        if let Some(endpoint) = endpoint {
            let mut state = endpoint
                .state
                .lock()
                .expect("sub-agent queue lock poisoned");
            state.collections.count += 1;
            state.collections.last = Some(Instant::now());
            self.pending_generation = Some(state.generation);
            if self.known_generation == Some(state.generation) {
                let page = SubagentQueuePage {
                    sequence: self.sequence + 1,
                    changes: Vec::new(),
                    has_more: false,
                };
                self.sequence = page.sequence;
                self.pending = Some(page.clone());
                return Ok(page);
            }
            for request in state.requests.values() {
                current.insert(
                    request.request_id.clone(),
                    SubagentQueueChange::Request(request.clone()),
                );
            }
            for receipt in state.completions.values() {
                current.insert(
                    receipt.request_id.clone(),
                    SubagentQueueChange::Completed(receipt.clone()),
                );
            }
        }
        let changes = current
            .iter()
            .filter(|(id, value)| self.known.get(*id) != Some(*value))
            .map(|(_, change)| change.clone())
            .chain(
                self.known
                    .keys()
                    .filter(|id| !current.contains_key(*id))
                    .map(|id| SubagentQueueChange::Retired {
                        request_id: id.clone(),
                    }),
            );
        let mut page = SubagentQueuePage {
            sequence: self.sequence + 1,
            changes: Vec::new(),
            has_more: false,
        };
        let mut bytes = 1024;
        for change in changes {
            let size = serde_json::to_vec(&change)?.len() + 1;
            anyhow::ensure!(
                size + 1024 <= mj_core::relay::RELAY_REPLAY_BYTE_BUDGET,
                "sub-agent queue entry exceeds the page budget"
            );
            if bytes + size > mj_core::relay::RELAY_REPLAY_BYTE_BUDGET {
                page.has_more = true;
                break;
            }
            bytes += size;
            page.changes.push(change);
        }
        self.sequence = page.sequence;
        self.pending = Some(page.clone());
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::subagent::SubagentToolAction;

    #[test]
    fn queue_changes_are_byte_bounded_and_completion_during_paging_is_not_lost() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        for i in 0..12 {
            endpoint
                .enqueue(SubagentToolRequest {
                    originating_command_id: None,
                    request_id: format!("request-{i:02}"),
                    created_at_ms: i,
                    action: SubagentToolAction::SendMessage {
                        child_session_id: "child".into(),
                        message: "λ".repeat(450_000),
                    },
                })
                .unwrap();
        }
        // Preparation can expose no endpoint before opening a persisted queue.
        // Its initial generation must not be mistaken for an observed empty one.
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        let mut feed = SubagentFeed::default();
        let preparing = feed.poll(None, None).unwrap();
        let first = feed
            .poll(Some(&endpoint), Some(preparing.sequence))
            .unwrap();
        assert!(first.has_more);
        assert_eq!(first, feed.poll(Some(&endpoint), None).unwrap());
        let first_id = first.changes[0].request_id().to_owned();
        endpoint
            .complete(SubagentToolResult {
                request_id: first_id.clone(),
                completed_at_ms: 20,
                is_error: false,
                message: "completed while its request page was in flight".into(),
            })
            .unwrap();
        let mut seen = BTreeMap::new();
        let mut page = first;
        loop {
            assert!(
                serde_json::to_vec(&page).unwrap().len() < mj_core::relay::RELAY_REPLAY_BYTE_BUDGET
            );
            for change in &page.changes {
                seen.insert(change.request_id().to_owned(), change.clone());
            }
            let more = page.has_more;
            page = feed.poll(Some(&endpoint), Some(page.sequence)).unwrap();
            if !more {
                break;
            }
        }
        assert_eq!(seen.len(), 12);
        assert!(matches!(seen[&first_id], SubagentQueueChange::Completed(_)));
        assert!(page.changes.is_empty());
    }
}
