use super::*;

/// Transcript projection parses every stored ACP content chunk. Keep that work
/// off the control task, and allow only a small number of projections to use
/// the blocking pool at once so a burst of active sessions cannot turn the
/// pool into an unbounded queue.
pub(super) const MAX_CONCURRENT_CONVERSATION_PROJECTIONS: usize = 2;
pub(super) const CONVERSATION_PROJECTION_CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConversationProjectionKey {
    pub(super) ordinal: u64,
    pub(super) digest: String,
}

impl ConversationProjectionKey {
    pub(super) fn of(materialized: &MaterializedSession) -> Self {
        Self {
            ordinal: materialized.applied_event_ordinal,
            digest: materialized.applied_event_digest.clone(),
        }
    }

    /// Event ordinals are monotonic. A changed digest at one ordinal is also a
    /// new projection, which preserves the integrity-repair path without
    /// relying on string ordering for digests.
    pub(super) fn is_newer_than(&self, other: &Self) -> bool {
        self.ordinal > other.ordinal
            || (self.ordinal == other.ordinal && self.digest != other.digest)
    }
}

pub(super) struct ConversationProjectionRequest {
    pub(super) materialized: MaterializedSession,
    pub(super) key: ConversationProjectionKey,
    pub(super) generation: u64,
}

pub(super) struct ConversationProjectionResult {
    pub(super) session_id: String,
    pub(super) key: ConversationProjectionKey,
    pub(super) generation: u64,
    pub(super) result: std::result::Result<BrowserTranscript, String>,
}

/// Owns the one-at-a-time/latest-state scheduling for each session. The
/// control loop remains the sole owner of these maps; background tasks only
/// return completed browser projections through `results`.
pub(super) struct ConversationProjectionDispatcher {
    pub(super) in_flight: std::collections::BTreeMap<String, (ConversationProjectionKey, u64)>,
    pub(super) pending: std::collections::BTreeMap<String, ConversationProjectionRequest>,
    pub(super) completed: std::collections::BTreeMap<String, ConversationProjectionKey>,
    pub(super) generations: std::collections::BTreeMap<String, u64>,
    pub(super) permits: Arc<tokio::sync::Semaphore>,
    pub(super) results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
}

impl ConversationProjectionDispatcher {
    pub(super) fn new(
        results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            in_flight: std::collections::BTreeMap::new(),
            pending: std::collections::BTreeMap::new(),
            completed: std::collections::BTreeMap::new(),
            generations: std::collections::BTreeMap::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_CONVERSATION_PROJECTIONS,
            )),
            results,
            shutdown,
        }
    }

    #[cfg(test)]
    pub(super) fn with_permits(
        results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
        shutdown: tokio_util::sync::CancellationToken,
        permits: usize,
    ) -> Self {
        let mut dispatcher = Self::new(results, shutdown);
        dispatcher.permits = Arc::new(tokio::sync::Semaphore::new(permits));
        dispatcher
    }

    /// Queue a session's newest durable view. At most one request is running
    /// and one newer request is retained for any given session.
    pub(super) fn enqueue(&mut self, materialized: MaterializedSession) {
        let session_id = materialized.session_id.clone();
        let key = ConversationProjectionKey::of(&materialized);
        if self
            .completed
            .get(&session_id)
            .is_some_and(|completed| !key.is_newer_than(completed))
        {
            return;
        }
        let generation = *self.generations.entry(session_id.clone()).or_default();
        if let Some((in_flight, in_flight_generation)) = self.in_flight.get(&session_id) {
            if generation != *in_flight_generation || key.is_newer_than(in_flight) {
                let replace = self.pending.get(&session_id).is_none_or(|pending| {
                    pending.generation != generation || key.is_newer_than(&pending.key)
                });
                if replace {
                    self.pending.insert(
                        session_id,
                        ConversationProjectionRequest {
                            materialized,
                            key,
                            generation,
                        },
                    );
                }
            }
            return;
        }
        self.in_flight
            .insert(session_id.clone(), (key.clone(), generation));
        self.start(ConversationProjectionRequest {
            materialized,
            key,
            generation,
        });
    }

    /// Finish one task and, when the session is still active, immediately
    /// launch the newest coalesced request. Returning `None` means the task
    /// failed or no longer belongs to the current in-flight request.
    pub(super) fn finish(
        &mut self,
        result: ConversationProjectionResult,
        session_active: bool,
    ) -> Option<(String, ConversationProjectionKey, BrowserTranscript)> {
        let expected = self.in_flight.remove(&result.session_id);
        if expected.as_ref() != Some(&(result.key.clone(), result.generation)) {
            tracing::warn!(
                session_id = %result.session_id,
                "discarding an out-of-date browser transcript projection"
            );
            return None;
        }
        let session_id = result.session_id;
        let key = result.key;
        let current_generation = self
            .generations
            .get(&session_id)
            .copied()
            .unwrap_or_default();
        let current = result.generation == current_generation;
        let projected = match result.result {
            Ok(transcript) if session_active && current => {
                self.completed.insert(session_id.clone(), key.clone());
                Some((session_id.clone(), key, transcript))
            }
            Ok(_) => {
                // An inactive session, or a result from an earlier lifecycle
                // generation, must not resurrect a conversation. Its next
                // active update gets a fresh generation.
                self.completed.remove(&session_id);
                None
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    "browser transcript projection failed: {error}"
                );
                None
            }
        };
        if session_active {
            if let Some(pending) = self.pending.remove(&session_id) {
                self.enqueue(pending.materialized);
            }
        } else {
            self.pending.remove(&session_id);
            self.completed.remove(&session_id);
        }
        projected
    }

    /// Drop queued/completed state after a controller reload removes a
    /// session. An in-flight task is allowed to finish; `finish` receives the
    /// current active-state guard and discards its result.
    pub(super) fn forget(&mut self, session_id: &str) {
        self.pending.remove(session_id);
        self.completed.remove(session_id);
        let generation = self.generations.entry(session_id.to_owned()).or_default();
        *generation = generation.wrapping_add(1);
    }

    pub(super) fn session_ids(&self) -> std::collections::BTreeSet<String> {
        self.in_flight
            .keys()
            .chain(self.pending.keys())
            .chain(self.completed.keys())
            .cloned()
            .collect()
    }

    pub(super) fn start(&self, request: ConversationProjectionRequest) {
        let session_id = request.materialized.session_id.clone();
        let key = request.key;
        let generation = request.generation;
        let permits = Arc::clone(&self.permits);
        let results = self.results.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let result = match tokio::select! {
                _ = shutdown.cancelled() => return,
                result = permits.acquire_owned() => result,
            } {
                Ok(permit) => {
                    let projection = tokio::task::spawn_blocking(move || {
                        mj_client::transcript::materialized_browser_transcript(
                            &request.materialized,
                        )
                    })
                    .await;
                    drop(permit);
                    projection
                        .map_err(|error| format!("transcript projection task failed: {error}"))
                }
                Err(error) => Err(format!("transcript projection worker stopped: {error}")),
            };
            let message = ConversationProjectionResult {
                session_id,
                key,
                generation,
                result,
            };
            tokio::select! {
                _ = shutdown.cancelled() => {}
                result = results.send(message) => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "browser transcript projection result dropped after server shutdown");
                    }
                }
            }
        });
    }
}
