//! Full replies are private data; the queue publishes only compact receipts.
use super::*;

impl SubagentEndpoint {
    pub(super) fn result_path(&self, request_id: &str) -> PathBuf {
        let digest = mj_core::hex::lower_hex(Sha256::digest(request_id.as_bytes()));
        self.path
            .parent()
            .expect("worker queue has a parent")
            .join("subagent-replies")
            .join(format!("{digest}.json"))
    }

    pub(super) fn write_result(&self, result: &SubagentToolResult) -> Result<()> {
        let path = self.result_path(&result.request_id);
        std::fs::create_dir_all(path.parent().expect("reply has a parent"))?;
        mj_core::config::atomic_write(&path, &serde_json::to_vec(result)?)
            .with_context(|| format!("persist sub-agent reply {}", result.request_id))
    }

    pub(super) fn read_result(
        &self,
        state: &QueueState,
        id: &str,
    ) -> Result<Option<SubagentToolResult>> {
        let Some(receipt) = state.completions.get(id) else {
            return Ok(None);
        };
        match std::fs::read(self.result_path(id)) {
            Ok(body) => Ok(Some(
                serde_json::from_slice(&body).context("decode sub-agent reply")?,
            )),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && state.caller_acknowledged.contains(id)
                    && state.daemon_acknowledged.contains(id) =>
            {
                // A consumed reply cannot authorize repeating its mutation.
                Ok(Some(SubagentToolResult {
                    request_id: id.to_owned(), completed_at_ms: receipt.completed_at_ms,
                    is_error: true, message: "This request already completed and its reply was acknowledged; the reply body has been retired. Do not repeat the operation.".into(),
                }))
            }
            Err(error) => Err(error).with_context(|| format!("read sub-agent reply {id}")),
        }
    }

    pub(super) fn import_legacy_results(&self) -> Result<()> {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        if state.results.is_empty() {
            return Ok(());
        }
        let mut next = state.clone();
        for (id, result) in std::mem::take(&mut next.results) {
            self.write_result(&result)?;
            next.completions
                .insert(id, SubagentCompletion::from(&result));
        }
        // Publish metadata only after every referenced body is durable. A crash
        // before this write simply imports the old queue again.
        self.persist(&mut next)?;
        *state = next;
        Ok(())
    }

    pub(super) fn acknowledge_replies(&self, ids: &[String], caller: bool) -> Result<()> {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        let mut next = state.clone();
        let mut changed = false;
        for id in ids {
            if !next.completions.contains_key(id) {
                continue;
            }
            changed |= if caller {
                next.caller_acknowledged.insert(id.clone())
            } else {
                next.daemon_acknowledged.insert(id.clone())
            };
        }
        if changed {
            // The receipt survives a crash after the body is removed.
            self.persist(&mut next)?;
            *state = next;
        }
        for id in ids {
            if state.caller_acknowledged.contains(id) && state.daemon_acknowledged.contains(id) {
                match std::fs::remove_file(self.result_path(id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| format!("retire sub-agent reply {id}"));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn retire_receipts(&self, state: &mut QueueState) -> Result<()> {
        // Bodies without both receipts remain durable. Backpressure at enqueue
        // bounds outstanding work; only acknowledged identities are evictable.
        while state.completions.len() > mj_core::relay::RELAY_RETRY_ID_RETENTION {
            let id = state
                .completions
                .values()
                .filter(|r| {
                    state.caller_acknowledged.contains(&r.request_id)
                        && state.daemon_acknowledged.contains(&r.request_id)
                })
                .min_by_key(|r| (r.completed_at_ms, &r.request_id))
                .map(|r| r.request_id.clone());
            let Some(id) = id else { break };
            // Both acknowledgments are already durable. Retry failed cleanup
            // before forgetting the identity, including after a crashed ACK.
            match std::fs::remove_file(self.result_path(&id)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("retire acknowledged sub-agent reply"),
            }
            state.completions.remove(&id);
            state.caller_acknowledged.remove(&id);
            state.daemon_acknowledged.remove(&id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::subagent::SubagentToolAction;

    #[test]
    fn reply_retirement_requires_both_receipts_and_survives_reopen() {
        for caller_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
            let request = SubagentToolRequest {
                originating_command_id: None,
                request_id: "durable-operation".into(),
                created_at_ms: 1,
                action: SubagentToolAction::ListAgents,
            };
            endpoint.enqueue(request.clone()).unwrap();
            let result = SubagentToolResult {
                request_id: request.request_id.clone(),
                completed_at_ms: 2,
                is_error: false,
                message: "reply".repeat(20_000),
            };
            let ids = [request.request_id.clone()];
            // A pending-tool status is not receipt of its eventual reply.
            endpoint.acknowledge_replies(&ids, true).unwrap();
            endpoint.complete(result.clone()).unwrap();
            endpoint.acknowledge_replies(&ids, caller_first).unwrap();
            let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
            assert_eq!(
                endpoint.enqueue(request.clone()).unwrap(),
                Some(result.clone())
            );
            endpoint
                .complete(SubagentToolResult {
                    message: "duplicate must not replace the reply".into(),
                    ..result.clone()
                })
                .unwrap();
            assert_eq!(
                endpoint.enqueue(request.clone()).unwrap(),
                Some(result.clone())
            );
            endpoint.acknowledge_replies(&ids, !caller_first).unwrap();
            assert!(!endpoint.result_path(&request.request_id).exists());
            let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
            assert!(
                endpoint
                    .enqueue(request)
                    .unwrap()
                    .unwrap()
                    .message
                    .contains("already completed")
            );
            assert!(
                endpoint.snapshot().0.is_empty(),
                "a retired reply must never requeue the operation"
            );
        }
    }
}
