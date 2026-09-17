use super::*;

pub(super) struct RelayEventPage {
    pub(super) events: Vec<RelayEvent>,
    pub(super) through_ordinal: u64,
    pub(super) through_digest: String,
}

/// A point-in-time view of the durable journal: everything needed to validate
/// a replay cursor and assemble a replay page, and nothing that requires the
/// relay lock to read.
///
/// Sealed segments are immutable and the active segment is append-only, so a
/// captured span list stays readable while the relay keeps recording events.
/// Sealing and garbage collection do invalidate it, so every read here is
/// written to fail loudly rather than return a short or torn page, and
/// `generation` lets the caller recognize that failure as a stale plan instead
/// of a real desynchronization.
pub struct RelayReplayPlan {
    spans: Vec<RelayJournalSpan>,
    /// Digests of recent live events and proven replay-page cursors, so the
    /// next sequential attachment validates without touching old segments.
    hot_digests: Vec<(u64, String)>,
    latest_ordinal: u64,
    latest_digest: String,
    acknowledged_through: u64,
    acknowledged_digest: String,
    recovery_floor_ordinal: u64,
    recovery_floor_digest: String,
    retained_through: u64,
    retained_digest: String,
    generation: u64,
}

/// A journal span could not be parsed during a replay. When newer readable
/// history exists past it, `read_events_after` attaches this to the error so
/// `attach` can answer with a `Desynchronized` recovery cursor rather than a
/// retryable failure that the controller would loop on forever.
#[derive(Debug)]
struct UnreadableRelaySpan {
    recover_after: u64,
}

/// An attach whose disk work has been lifted out of the relay lock.
///
/// [`DurableRelay::take_deferred_attach`] builds one while holding the lock;
/// [`Self::finish`] then does the reading, decompressing and page assembly
/// with the lock released, so live event recording keeps running underneath a
/// controller's catch-up.
pub struct DeferredRelayAttach {
    request_id: String,
    protocol_version: u32,
    plan: RelayReplayPlan,
    state: RelayOperationalState,
    after_ordinal: u64,
    after_digest: String,
}

impl DurableRelay {
    /// Capture everything a replay page needs from this relay, so the file
    /// reads and gzip decompression behind it can run without the relay lock.
    pub(super) fn replay_plan(&self) -> RelayReplayPlan {
        RelayReplayPlan {
            spans: self.journal_spans.clone(),
            hot_digests: self
                .hot_events
                .iter()
                .map(|event| (event.ordinal, event.digest.clone()))
                .chain(self.replay_cursors.iter().cloned())
                .collect(),
            latest_ordinal: self.snapshot.latest_ordinal,
            latest_digest: self.snapshot.latest_digest.clone(),
            acknowledged_through: self.snapshot.acknowledged_through,
            acknowledged_digest: self.snapshot.acknowledged_digest.clone(),
            recovery_floor_ordinal: self.snapshot.recovery_floor_ordinal,
            recovery_floor_digest: self.snapshot.recovery_floor_digest.clone(),
            retained_through: self.snapshot.retained_through(),
            retained_digest: self.snapshot.retained_digest().to_owned(),
            generation: self.journal_generation,
        }
    }

    /// Retain a cursor this worker just proved while reading a replay page.
    /// This is an optimization only: losing the cache causes another journal
    /// validation scan, never a loss of durable history.
    pub fn remember_replay_cursor(&mut self, response: &RelayResponseEnvelope) {
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    through_ordinal,
                    through_digest,
                    ..
                },
        } = &response.body
        else {
            return;
        };
        if self
            .replay_cursors
            .back()
            .is_some_and(|(ordinal, digest)| ordinal == through_ordinal && digest == through_digest)
        {
            return;
        }
        self.replay_cursors
            .retain(|(ordinal, _)| ordinal != through_ordinal);
        if self.replay_cursors.len() == RELAY_REPLAY_CURSOR_CAPACITY {
            self.replay_cursors.pop_front();
        }
        self.replay_cursors
            .push_back((*through_ordinal, through_digest.clone()));
    }

    /// Split an attach into the cheap part that needs the relay lock and the
    /// expensive part that does not.
    ///
    /// Catch-up over a long offline history reads page after page from disk
    /// and decompresses sealed segments. Doing that under the relay lock
    /// blocks live event recording until it finishes, which is exactly what a
    /// controller attaching is not supposed to cost the session. `None` means
    /// this envelope is not an attach, or is one that cannot be served at all;
    /// the caller falls back to [`Self::handle`], which answers it.
    pub fn take_deferred_attach(
        &self,
        envelope: &RelayRequestEnvelope,
    ) -> Option<DeferredRelayAttach> {
        let RelayRequest::Attach {
            after_ordinal,
            after_digest,
        } = &envelope.request
        else {
            return None;
        };
        if self.envelope_rejection(envelope).is_some() {
            return None;
        }
        let state = self.operational_state();
        Some(DeferredRelayAttach {
            request_id: envelope.request_id.clone(),
            protocol_version: envelope.protocol_version,
            plan: self.replay_plan(),
            state,
            after_ordinal: *after_ordinal,
            after_digest: after_digest.clone(),
        })
    }
}

impl std::fmt::Display for UnreadableRelaySpan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "relay history is unreadable; readable events resume after event {}",
            self.recover_after
        )
    }
}

impl std::error::Error for UnreadableRelaySpan {}

impl RelayReplayPlan {
    pub(super) fn attach(
        &self,
        after_ordinal: u64,
        after_digest: &str,
        state: RelayOperationalState,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        if let Err(error) = self.validate_cursor(after_ordinal, after_digest) {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::Desynchronized,
                error.to_string(),
                false,
                Some(self.desynchronized_detail(after_ordinal, after_digest)),
            )));
        }
        let page =
            match self.read_events_after(after_ordinal, after_digest, RELAY_REPLAY_BYTE_BUDGET) {
                Ok(page) => page,
                Err(error) => {
                    // An unreadable old span cannot be served, but newer history
                    // still can. Answer with a desynchronization cursor past the
                    // corruption so the controller resynchronizes from there
                    // instead of retrying the same unparseable bytes forever.
                    if let Some(gap) = error.downcast_ref::<UnreadableRelaySpan>() {
                        let (earliest_available, earliest_digest) =
                            self.recovery_cursor_after(gap.recover_after);
                        return Ok(Err(relay_protocol_error(
                            RelayErrorCode::Desynchronized,
                            error.to_string(),
                            false,
                            Some(RelayErrorDetail::Desynchronized {
                                requested_after: after_ordinal,
                                requested_digest: after_digest.to_owned(),
                                earliest_available,
                                earliest_digest,
                                latest: self.latest_ordinal,
                                latest_digest: self.latest_digest.clone(),
                            }),
                        )));
                    }
                    return Err(error);
                }
            };
        ensure_serialized_budget(&state, RELAY_STATE_BYTE_BUDGET, "relay operational state")?;
        Ok(Ok(RelayResponsePayload::Attached {
            state,
            events: page.events,
            through_ordinal: page.through_ordinal,
            through_digest: page.through_digest,
        }))
    }

    /// A resync cursor the controller can attach at to recover history past an
    /// unreadable span. It must name an ordinal whose digest is resolvable
    /// *without* the corrupt span, so it points at the first readable, self-
    /// valid event strictly after the corruption. The controller re-attaches
    /// after it and recovers every later event; at most the corrupt span and
    /// that one boundary event are lost. When nothing readable remains, it
    /// resumes live at the frontier.
    fn recovery_cursor_after(&self, corrupt_last_ordinal: u64) -> (u64, String) {
        for span in &self.spans {
            if span.file_last_ordinal <= corrupt_last_ordinal {
                continue;
            }
            let mut found = None;
            let _ = visit_relay_journal_file(&span.path, JournalReadMode::Recover, |event, _| {
                if event.ordinal > corrupt_last_ordinal && validate_relay_event_self(&event).is_ok()
                {
                    found = Some((event.ordinal, event.digest));
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            });
            if let Some(cursor) = found {
                return cursor;
            }
        }
        (self.latest_ordinal, self.latest_digest.clone())
    }

    pub(super) fn validate_cursor(&self, after_ordinal: u64, after_digest: &str) -> Result<()> {
        if after_ordinal < self.retained_through {
            bail!(
                "event {after_ordinal} is no longer available; relay retained events after {}",
                self.retained_through
            );
        }
        if after_ordinal > self.latest_ordinal {
            bail!(
                "event {after_ordinal} is newer than relay frontier {}",
                self.latest_ordinal
            );
        }
        validate_relay_digest(after_digest, "event cursor digest")?;
        let expected = self
            .digest_at(after_ordinal)?
            .ok_or_else(|| anyhow!("relay digest missing at event {after_ordinal}"))?;
        if after_digest != expected {
            bail!("event {after_ordinal} digest does not match the relay event chain");
        }
        Ok(())
    }

    pub(super) fn digest_at(&self, ordinal: u64) -> Result<Option<String>> {
        if ordinal == 0 {
            return Ok(Some(RELAY_EVENT_GENESIS_DIGEST.to_owned()));
        }
        if ordinal == self.latest_ordinal {
            return Ok(Some(self.latest_digest.clone()));
        }
        if ordinal == self.acknowledged_through {
            return Ok(Some(self.acknowledged_digest.clone()));
        }
        if ordinal == self.recovery_floor_ordinal {
            return Ok(Some(self.recovery_floor_digest.clone()));
        }
        if let Some((_, digest)) = self.hot_digests.iter().find(|(hot, _)| *hot == ordinal) {
            return Ok(Some(digest.clone()));
        }
        // A v1 span caches the digest at its boundary in the first event's
        // `previous_digest`, so the digest before the span can be read without
        // touching the segment. v2 records carry no such back-reference
        // (`file_first_previous_digest` is None), so fall through to read the
        // event at `ordinal` directly.
        if let Some(span) = self.spans.iter().find(|span| {
            span.file_first_ordinal.checked_sub(1) == Some(ordinal) && ordinal >= span.after_ordinal
        }) {
            if let Some(digest) = &span.file_first_previous_digest {
                return Ok(Some(digest.clone()));
            }
            let mut digest = None;
            visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
                validate_relay_event_self(&event)
                    .with_context(|| format!("validate relay journal {}", span.path.display()))?;
                if event.format == RELAY_EVENT_FORMAT_V1 && !event.previous_digest.is_empty() {
                    digest = Some(event.previous_digest);
                }
                Ok(ControlFlow::Break(()))
            })?;
            if digest.is_some() {
                return Ok(digest);
            }
            // v2 first event: no cached boundary digest; read the target itself.
        }
        let Some(span) = self
            .spans
            .iter()
            .find(|span| ordinal > span.after_ordinal && ordinal <= span.file_last_ordinal)
        else {
            return Ok(None);
        };
        let mut digest = None;
        let mut previous: Option<RelayEvent> = None;
        visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
            // Each record is validated by its own digest (the corruption check).
            // Between consecutive records the v1 chain link is also enforced;
            // v2 records have no link and are trusted on their own digest.
            validate_relay_event_self(&event)
                .with_context(|| format!("validate relay journal {}", span.path.display()))?;
            if let Some(previous) = &previous
                && event.format == RELAY_EVENT_FORMAT_V1
                && event.previous_digest != previous.digest
            {
                bail!(
                    "relay journal {} event {} does not chain from event {}",
                    span.path.display(),
                    event.ordinal,
                    previous.ordinal
                );
            }
            if event.ordinal == ordinal {
                digest = Some(event.digest.clone());
                return Ok(ControlFlow::Break(()));
            }
            previous = Some(event);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(digest)
    }

    pub(super) fn read_events_after(
        &self,
        after_ordinal: u64,
        after_digest: &str,
        byte_budget: usize,
    ) -> Result<RelayEventPage> {
        let mut events = Vec::new();
        let mut used = 0_usize;
        let mut through_ordinal = after_ordinal;
        // `attach` validated this exact cursor immediately before entering the
        // reader. Reusing it avoids a second decompression pass over the
        // cursor's sealed segment.
        let mut through_digest = after_digest.to_owned();
        let mut page_full = false;

        for span in &self.spans {
            if page_full || span.file_last_ordinal <= through_ordinal {
                continue;
            }
            let read = visit_relay_journal_file(
                &span.path,
                JournalReadMode::Strict,
                |event, encoded_len| {
                    if event.ordinal <= span.after_ordinal || event.ordinal <= through_ordinal {
                        return Ok(ControlFlow::Continue(()));
                    }
                    if event.ordinal > self.latest_ordinal {
                        // The active segment kept growing after this plan was
                        // captured. Those events are real, but the reply's
                        // operational state describes the frontier the plan saw,
                        // so the page stops there and the caller asks again.
                        return Ok(ControlFlow::Break(()));
                    }
                    let expected = through_ordinal
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
                    if event.ordinal != expected {
                        bail!(
                            "relay journal page has a gap after event {through_ordinal}: found {}",
                            event.ordinal
                        );
                    }
                    // The page is assembled off the relay lock, so it carries its
                    // own proof that it is one unbroken run the cursor named rather
                    // than fragments of a journal that moved. For v1 the in-record
                    // chain link proves this; a v2 page relies instead on both
                    // endpoints being digest anchors (the cursor and the frontier)
                    // plus each interior record self-validating and the ordinals
                    // being contiguous.
                    if event.format == RELAY_EVENT_FORMAT_V1
                        && event.previous_digest != through_digest
                    {
                        bail!(
                            "relay journal event {} does not chain from event {through_ordinal}",
                            event.ordinal
                        );
                    }
                    validate_relay_event(through_ordinal, &through_digest, &event)
                        .context("validate relay journal page event")?;
                    if !events.is_empty() && used.saturating_add(encoded_len) > byte_budget {
                        page_full = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    used = used.saturating_add(encoded_len);
                    through_ordinal = event.ordinal;
                    through_digest.clone_from(&event.digest);
                    events.push(event);
                    Ok(ControlFlow::Continue(()))
                },
            );
            if let Err(error) = read {
                // This span will not parse. If newer, readable history exists
                // past it, mark the error so `attach` can send the controller a
                // recovery cursor after this span instead of a retryable failure
                // it would loop on. The corrupt bytes are never served as valid.
                if span.file_last_ordinal < self.latest_ordinal {
                    return Err(error.context(UnreadableRelaySpan {
                        recover_after: span.file_last_ordinal,
                    }));
                }
                return Err(error);
            }
            // A canonical span contributes every ordinal through its last one.
            // Stopping short means the file no longer holds what this plan
            // captured: it was sealed, rewritten, or pruned under the reader.
            if !page_full && through_ordinal < span.file_last_ordinal.min(self.latest_ordinal) {
                bail!(
                    "relay journal {} no longer covers event {}",
                    span.path.display(),
                    span.file_last_ordinal
                );
            }
        }
        if !page_full
            && (through_ordinal != self.latest_ordinal || through_digest != self.latest_digest)
        {
            // The spans end at the frontier this plan captured. Anything else
            // is a page assembled from a journal that moved, never a short
            // answer a caller could mistake for a complete one.
            bail!(
                "relay journal ended at event {through_ordinal}, expected frontier {}",
                self.latest_ordinal
            );
        }
        Ok(RelayEventPage {
            events,
            through_ordinal,
            through_digest,
        })
    }

    pub(super) fn desynchronized_detail(
        &self,
        requested_after: u64,
        requested_digest: &str,
    ) -> RelayErrorDetail {
        RelayErrorDetail::Desynchronized {
            requested_after,
            requested_digest: requested_digest.to_owned(),
            earliest_available: self.retained_through,
            earliest_digest: self.retained_digest.clone(),
            latest: self.latest_ordinal,
            latest_digest: self.latest_digest.clone(),
        }
    }
}

impl DeferredRelayAttach {
    /// The journal generation this attach was planned against. Compare it with
    /// [`DurableRelay::journal_generation`] after [`Self::finish`] fails: an
    /// unchanged generation means the failure is real, and a changed one means
    /// the journal was resealed or collected mid-read and the controller
    /// should simply attach again.
    pub fn journal_generation(&self) -> u64 {
        self.plan.generation
    }

    /// Blocking: reads journal segments and decompresses sealed ones. Callers
    /// on an async runtime must run this off the event loop.
    pub fn finish(self) -> RelayResponseEnvelope {
        let body = match self
            .plan
            .attach(self.after_ordinal, &self.after_digest, self.state)
        {
            Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
            Ok(Err(error)) => RelayResponseBody::Error { error },
            Err(error) => RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: format!("{error:#}"),
                    retryable: true,
                    detail: None,
                },
            },
        };
        RelayResponseEnvelope {
            request_id: self.request_id,
            protocol_version: self.protocol_version,
            body,
        }
    }

    /// The answer for an attach whose plan went stale while it was reading:
    /// nothing is wrong with the controller's cursor, so it retries against
    /// the journal as it now stands.
    pub fn stale_journal_response(
        request_id: String,
        protocol_version: u32,
    ) -> RelayResponseEnvelope {
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body: RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: "relay journal was resealed or collected while the replay page was \
                              being read; attach again"
                        .to_owned(),
                    retryable: true,
                    detail: None,
                },
            },
        }
    }
}
