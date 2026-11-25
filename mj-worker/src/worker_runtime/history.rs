//! Transient history requests. No history payload is written to worker state.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use mj_core::history::*;
use tokio::sync::oneshot;

struct Pending {
    request: HistoryRequest,
    expires: Instant,
    reply: oneshot::Sender<HistoryResult>,
}

#[derive(Clone)]
pub(super) struct HistoryEndpoint {
    pending: Arc<Mutex<BTreeMap<String, Pending>>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for HistoryEndpoint {
    fn default() -> Self {
        Self {
            pending: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        }
    }
}

impl HistoryEndpoint {
    #[cfg(test)]
    pub fn requests(&self) -> Vec<HistoryRequest> {
        self.requests_for_protocol(mj_core::relay::RELAY_PROTOCOL_VERSION)
    }

    /// Pending requests a controller speaking `protocol_version` can parse.
    /// A controller older than `search_history` would fail to deserialize the
    /// whole `HistoryRequests` reply, so such requests are failed here and
    /// never listed.
    pub fn requests_for_protocol(&self, protocol_version: u32) -> Vec<HistoryRequest> {
        let mut pending = self.pending.lock().expect("history queue lock poisoned");
        pending.retain(|_, entry| entry.expires > Instant::now() && !entry.reply.is_closed());
        if protocol_version < mj_core::relay::RELAY_SEARCH_HISTORY_PROTOCOL {
            let unsupported: Vec<String> = pending
                .iter()
                .filter(|(_, entry)| {
                    matches!(entry.request.query, HistoryQuery::SearchHistory { .. })
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in unsupported {
                if let Some(entry) = pending.remove(&id) {
                    let result = HistoryResult::failed(
                        id,
                        format!(
                            "search_history requires controller relay protocol {} (connected controller uses {}); upgrade or restart the daemon",
                            mj_core::relay::RELAY_SEARCH_HISTORY_PROTOCOL,
                            protocol_version
                        ),
                    );
                    if entry.reply.send(result).is_err() {
                        tracing::debug!("history caller disconnected before its response");
                    }
                }
            }
        }
        pending
            .values()
            .map(|entry| entry.request.clone())
            .collect()
    }

    pub fn complete(&self, result: HistoryResult) {
        if let Some(entry) = self
            .pending
            .lock()
            .expect("history queue lock poisoned")
            .remove(&result.request_id)
            && entry.reply.send(result).is_err()
        {
            tracing::debug!("history caller disconnected before its response");
        }
    }

    pub async fn query(&self, query: HistoryQuery) -> Result<HistoryResult> {
        query.validate()?;
        let _permit = self
            .permits
            .try_acquire()
            .context("too many pending history queries; retry shortly")?;
        let id = mj_core::state::new_session_id()?;
        let deadline = tokio::time::Instant::now() + QUERY_TIMEOUT;
        let (reply, received) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("history queue lock poisoned");
            pending.retain(|_, entry| entry.expires > Instant::now() && !entry.reply.is_closed());
            ensure!(
                pending.len() < MAX_PENDING,
                "too many pending history queries; retry shortly"
            );
            pending.insert(
                id.clone(),
                Pending {
                    request: HistoryRequest {
                        request_id: id.clone(),
                        query,
                    },
                    expires: deadline.into_std(),
                    reply,
                },
            );
        }
        struct Cleanup(HistoryEndpoint, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0
                    .pending
                    .lock()
                    .expect("history queue lock poisoned")
                    .remove(&self.1);
            }
        }
        let _cleanup = Cleanup(self.clone(), id);
        tokio::time::timeout_at(deadline, received)
            .await
            .context("history query timed out; controller may be disconnected")?
            .context("history response channel closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn disconnected_controller_queries_expire_and_release_capacity() {
        let endpoint = HistoryEndpoint::default();
        let mut tasks = Vec::new();
        for _ in 0..MAX_PENDING {
            let endpoint = endpoint.clone();
            tasks.push(mj_core::runtime::spawn(async move {
                endpoint
                    .query(HistoryQuery::GetSessionBrief {
                        session_id: "session".into(),
                        max_chars: 16_000,
                    })
                    .await
            }));
        }
        while endpoint.requests().len() != MAX_PENDING {
            tokio::task::yield_now().await;
        }
        assert!(
            endpoint
                .query(HistoryQuery::GetSessionBrief {
                    session_id: "session".into(),
                    max_chars: 16_000,
                },)
                .await
                .unwrap_err()
                .to_string()
                .contains("too many")
        );
        tokio::time::advance(QUERY_TIMEOUT + std::time::Duration::from_secs(1)).await;
        for task in tasks {
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
        }
        assert!(endpoint.requests().is_empty());
        assert_eq!(endpoint.permits.available_permits(), MAX_PENDING);
    }
    #[tokio::test]
    async fn history_replies_are_delivered_and_cancelled_requests_do_not_accumulate() {
        let endpoint = HistoryEndpoint::default();
        let request = HistoryQuery::SearchSessions {
            query: "needle".into(),
            limit: 20,
        };
        let cloned = endpoint.clone();
        let pending = mj_core::runtime::spawn(async move { cloned.query(request).await });
        let requests = loop {
            let requests = endpoint.requests();
            if !requests.is_empty() {
                break requests;
            }
            tokio::task::yield_now().await;
        };
        endpoint.complete(HistoryResult {
            request_id: requests[0].request_id.clone(),
            value: serde_json::json!({"text":"x".repeat(100_000)}),
            is_error: false,
        });
        assert_eq!(
            pending.await.unwrap().unwrap().value["text"]
                .as_str()
                .unwrap()
                .len(),
            100_000
        );
        assert!(endpoint.requests().is_empty());
        let cloned = endpoint.clone();
        let pending = mj_core::runtime::spawn(async move {
            cloned
                .query(HistoryQuery::GetSessionBrief {
                    session_id: "session".into(),
                    max_chars: 16_000,
                })
                .await
        });
        while endpoint.requests().is_empty() {
            tokio::task::yield_now().await;
        }
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(endpoint.requests().is_empty());
    }

    #[tokio::test]
    async fn older_controllers_get_search_history_failed_and_still_poll_legacy_requests() {
        let endpoint = HistoryEndpoint::default();
        let search = {
            let endpoint = endpoint.clone();
            mj_core::runtime::spawn(async move {
                endpoint
                    .query(HistoryQuery::SearchHistory {
                        query: "needle".into(),
                        session_ids: None,
                        session_id: None,
                        role: None,
                        order: None,
                        limit: 20,
                        context_messages: 0,
                        max_chars: 16_000,
                        cursor: None,
                    })
                    .await
            })
        };
        let legacy = {
            let endpoint = endpoint.clone();
            mj_core::runtime::spawn(async move {
                endpoint
                    .query(HistoryQuery::SearchSessions {
                        query: "needle".into(),
                        limit: 20,
                    })
                    .await
            })
        };
        while endpoint.requests().len() != 2 {
            tokio::task::yield_now().await;
        }
        let older = mj_core::relay::RELAY_SEARCH_HISTORY_PROTOCOL - 1;
        let listed = endpoint.requests_for_protocol(older);
        assert_eq!(listed.len(), 1);
        assert!(matches!(
            listed[0].query,
            HistoryQuery::SearchSessions { .. }
        ));
        // The reply serializes with only variants an older controller knows.
        let wire = serde_json::to_string(&listed).unwrap();
        assert!(!wire.contains("search_history"));
        let failed = search.await.unwrap().unwrap();
        assert!(failed.is_error);
        let error = failed.value["error"].as_str().unwrap();
        assert!(
            error.contains("requires controller relay protocol 92"),
            "{error}"
        );
        assert!(error.contains("connected controller uses 91"), "{error}");
        assert!(!legacy.is_finished());
        endpoint.complete(HistoryResult {
            request_id: listed[0].request_id.clone(),
            value: serde_json::json!({"ok": true}),
            is_error: false,
        });
        assert!(!legacy.await.unwrap().unwrap().is_error);
        assert!(endpoint.requests().is_empty());
    }
}
