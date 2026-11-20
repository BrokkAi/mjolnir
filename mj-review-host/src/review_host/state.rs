//! The one owner of each session's review state.
//!
//! A session's stored row holds its declined findings, its outstanding
//! findings verdicts and the reviews it has open. The host's task is the only
//! writer: it keeps the first two here, takes the third from its slots, and
//! composes the whole row at the moment it queues a save. The persistence lane
//! is first in, first out, so the stored row always equals a state this task
//! held, in order. Nothing else loads a row, changes it and saves it back, so
//! one review's save cannot undo another's change.
//!
//! The same task decides which repository roots may change: a review owns the
//! roots its capture returned from the moment it opens until it closes, and an
//! operation that is about to change refs reserves its roots first. Two
//! reviews of a session never share a root, and an override or baseline move
//! never runs against a root a review owns.

use super::*;

/// What owns a root another request asked for.
pub enum Overlap {
    /// An open review, by the roots it covers.
    Review {
        root: std::path::PathBuf,
        review_roots: Vec<std::path::PathBuf>,
    },
    /// An operation that is changing refs.
    Operation {
        label: &'static str,
        root: Option<std::path::PathBuf>,
    },
}

/// Whether a session's worker takes part in concurrent reviews: it can
/// resolve a path to a root without a capture, and its tool selects a verdict
/// by path. A session below it keeps one open review at a time.
pub fn concurrent_reviews(protocol: u32) -> bool {
    protocol >= mj_core::relay::RELAY_CONCURRENT_REVIEW_PROTOCOL
}

fn list(roots: &[std::path::PathBuf]) -> String {
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl HostState {
    // ---- lookups -------------------------------------------------------

    /// The session's open reviews, oldest key first.
    pub fn keys_of(&self, session_id: &str) -> Vec<ReviewKey> {
        self.reviews
            .keys()
            .filter(|key| key.session_id == session_id)
            .cloned()
            .collect()
    }

    pub fn has_slot(&self, session_id: &str) -> bool {
        self.reviews.keys().any(|key| key.session_id == session_id)
    }

    /// The review a result belongs to: the session's review with this epoch.
    /// A result whose review was cancelled, resolved or replaced finds none.
    pub fn key_for_epoch(&self, session_id: &str, epoch: u64) -> Option<ReviewKey> {
        self.reviews
            .iter()
            .find(|(key, slot)| key.session_id == session_id && slot.epoch == epoch)
            .map(|(key, _)| key.clone())
    }

    /// The session's review that has a reviewer child registered.
    pub fn key_for_child(&self, session_id: &str, child: &str) -> Option<ReviewKey> {
        self.reviews
            .iter()
            .find(|(key, slot)| {
                key.session_id == session_id
                    && slot.open.active_group.as_ref().is_some_and(|group| {
                        group
                            .registrations
                            .iter()
                            .any(|item| item.child_session_id.as_deref() == Some(child))
                    })
            })
            .map(|(key, _)| key.clone())
    }

    /// Whether the session has a review open, being opened, being prepared or
    /// being changed by an operation.
    pub fn has_open_work(&self, session_id: &str) -> bool {
        self.has_slot(session_id)
            || self
                .pending_open
                .keys()
                .any(|key| key.session_id == session_id)
            || self
                .preparations
                .values()
                .any(|preparation| preparation.session_id == session_id)
            || self
                .operations
                .values()
                .any(|operation| operation.session_id == session_id)
    }

    // ---- the stored row ------------------------------------------------

    /// Takes a session's stored state as the host's own, unless the host
    /// already holds it. What the host holds is newer than any read: every
    /// save of the row came from here.
    pub fn adopt(&mut self, session_id: &str, loaded: &TurnReviewState) {
        self.review_state
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionReviews {
                part: TurnReviewState {
                    declined_findings: loaded.declined_findings.clone(),
                    push_findings: loaded.push_findings.clone(),
                    open_reviews: Vec::new(),
                },
                ..SessionReviews::default()
            });
    }

    /// The row to save: the session part and every review the row should
    /// list, in the order they opened. A review whose removal was queued is
    /// left out, and one being opened is in, so a save never undoes another.
    pub fn compose(&self, session_id: &str) -> Result<TurnReviewState, String> {
        self.compose_for(session_id, None)
    }

    /// [`Self::compose`] for the save of `own`'s outcome. Any other save
    /// lists a review whose outcome is still being saved as it last was
    /// durable, so a settled role is never stored without the text it was
    /// settled from.
    pub fn compose_for(
        &self,
        session_id: &str,
        own: Option<&ReviewKey>,
    ) -> Result<TurnReviewState, String> {
        let owner = self
            .review_state
            .get(session_id)
            .ok_or_else(|| format!("the review state of session {session_id} is not loaded"))?;
        let mut open: Vec<(u64, OpenReview)> = self
            .reviews
            .iter()
            .map(|(key, slot)| {
                let open = match &slot.pending_outcome {
                    Some(pending) if own != Some(key) => &pending.prior,
                    _ => &slot.open,
                };
                (key, slot, open)
            })
            .filter(|(key, slot, open)| {
                key.session_id == session_id && !slot.leaving_row && open.is_stored()
            })
            .map(|(_, slot, open)| (slot.epoch, open.clone()))
            .chain(
                self.pending_open
                    .iter()
                    .filter(|(key, _)| key.session_id == session_id)
                    .map(|(_, pending)| (pending.epoch, pending.open.clone())),
            )
            .collect();
        open.sort_by_key(|(epoch, _)| *epoch);
        let mut state = owner.part.clone();
        state.open_reviews = open.into_iter().map(|(_, review)| review).collect();
        Ok(state)
    }

    /// Queues a save of the composed row.
    pub fn persist(
        &self,
        session_id: &str,
        completion: Option<PersistenceCompletion>,
    ) -> Result<(), String> {
        let state = self.compose(session_id)?;
        self.persistence
            .as_ref()
            .ok_or_else(|| "the review persistence lane stopped".to_owned())?
            .send(PersistenceRequest::Save {
                session_id: session_id.to_owned(),
                state: Box::new(state),
                completion,
            })
            .map_err(|_| "the review persistence lane stopped".to_owned())
    }

    /// Queues a save of the composed row whose acknowledgement the receiver
    /// reports, for an operation that must not go on before the row is
    /// durable.
    pub fn persist_acknowledged(
        &self,
        operation: &'static str,
        session_id: &str,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let state = self.compose(session_id)?;
        let (reply, saved) = oneshot::channel();
        self.persistence
            .as_ref()
            .ok_or_else(|| "the review persistence lane stopped".to_owned())?
            .send(PersistenceRequest::SaveAcknowledged {
                operation,
                session_id: session_id.to_owned(),
                state: Box::new(state),
                reply,
            })
            .map_err(|_| "the review persistence lane stopped".to_owned())?;
        Ok(saved)
    }

    /// Ends a review: removes its slot and counts the close, which can have
    /// moved a base.
    pub fn drop_slot(&mut self, key: &ReviewKey) -> Option<ReviewSlot> {
        let slot = self.reviews.remove(key)?;
        self.moved_base(&key.session_id);
        Some(slot)
    }

    /// Counts a change that can have moved one of the session's review bases.
    pub fn moved_base(&mut self, session_id: &str) {
        if let Some(owner) = self.review_state.get_mut(session_id) {
            owner.base_generation = owner.base_generation.saturating_add(1);
        }
    }

    pub fn base_generation(&self, session_id: &str) -> u64 {
        self.review_state
            .get(session_id)
            .map_or(0, |owner| owner.base_generation)
    }

    /// Forgets the state of sessions that have nothing open. Anything read
    /// again later equals what was saved, because the lane is in order.
    pub fn release_idle_state(&mut self, live: &BTreeSet<String>) {
        let sessions: Vec<_> = self.review_state.keys().cloned().collect();
        for session_id in sessions {
            if !live.contains(&session_id)
                && !self.has_open_work(&session_id)
                && !self.loading.contains_key(&session_id)
            {
                self.review_state.remove(&session_id);
            }
        }
    }

    // ---- loading -------------------------------------------------------

    /// Hands the event back when the session's state is held, and otherwise
    /// keeps it until the state is read.
    pub fn when_loaded(&mut self, session_id: &str, event: HostEvent) -> Option<HostEvent> {
        if self.review_state.contains_key(session_id) {
            return Some(event);
        }
        let waiting = self.loading.entry(session_id.to_owned()).or_default();
        let first = waiting.is_empty();
        waiting.push(event);
        if first {
            self.load_session(session_id);
        }
        None
    }

    fn load_session(&mut self, session_id: &str) {
        let (reply, loaded) = oneshot::channel();
        let queued = self
            .persistence
            .as_ref()
            .ok_or_else(|| "the review persistence lane stopped".to_owned())
            .and_then(|lane| {
                lane.send(PersistenceRequest::Load {
                    session_id: session_id.to_owned(),
                    reply,
                })
                .map_err(|_| "the review persistence lane stopped".to_owned())
            });
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        mj_core::runtime::spawn(async move {
            let result = match queued {
                Ok(()) => loaded
                    .await
                    .unwrap_or_else(|_| Err("the review persistence lane stopped".to_owned())),
                Err(error) => Err(error),
            };
            let _ = events.send(HostEvent::SessionLoaded { session_id, result });
        });
    }

    pub async fn session_loaded(
        &mut self,
        session_id: String,
        result: Result<TurnReviewState, String>,
    ) {
        let waiting = self.loading.remove(&session_id).unwrap_or_default();
        match result {
            Ok(loaded) => {
                self.adopt(&session_id, &loaded);
                for event in waiting {
                    let _ = Box::pin(self.handle(event)).await;
                }
            }
            Err(error) => {
                for event in waiting {
                    refuse(event, &format!("could not read the review state: {error}"));
                }
            }
        }
    }

    // ---- who may change which root -------------------------------------

    /// Which open review, pending review or reservation owns the first of
    /// `roots`, in the order they were asked for, that is owned.
    pub fn first_overlap(&self, session_id: &str, roots: &[std::path::PathBuf]) -> Option<Overlap> {
        let owner = self.review_state.get(session_id);
        for root in roots {
            let reviews = self
                .reviews
                .iter()
                .filter(|(key, _)| key.session_id == session_id)
                .map(|(_, slot)| slot.roots())
                .chain(
                    self.pending_open
                        .iter()
                        .filter(|(key, _)| key.session_id == session_id)
                        .map(|(_, pending)| {
                            pending
                                .driver
                                .deltas()
                                .iter()
                                .map(|delta| delta.root.clone())
                                .collect::<Vec<_>>()
                        }),
                );
            for review_roots in reviews {
                if review_roots.contains(root) {
                    return Some(Overlap::Review {
                        root: root.clone(),
                        review_roots,
                    });
                }
            }
            for reservation in owner.iter().flat_map(|owner| &owner.reservations) {
                match &reservation.roots {
                    ReservedRoots::Everything => {
                        return Some(Overlap::Operation {
                            label: reservation.label,
                            root: None,
                        });
                    }
                    ReservedRoots::Roots(held) if held.contains(root) => {
                        return Some(Overlap::Operation {
                            label: reservation.label,
                            root: Some(root.clone()),
                        });
                    }
                    ReservedRoots::Roots(_) => {}
                }
            }
        }
        // A reservation that covers everything owns a session with no roots
        // asked for, too.
        owner
            .iter()
            .flat_map(|owner| &owner.reservations)
            .find(|reservation| matches!(reservation.roots, ReservedRoots::Everything))
            .map(|reservation| Overlap::Operation {
                label: reservation.label,
                root: None,
            })
    }

    /// Whether anything at all of the session's owns a root: a review, one
    /// being opened, or a reservation.
    fn owns_any_root(&self, session_id: &str) -> bool {
        self.reviews.keys().any(|key| key.session_id == session_id)
            || self
                .pending_open
                .keys()
                .any(|key| key.session_id == session_id)
            || self
                .review_state
                .get(session_id)
                .is_some_and(|owner| !owner.reservations.is_empty())
    }

    /// The decision to open a prepared review, made once and in the actor,
    /// where nothing else can run between it and the open: the refusal, or
    /// `None` when the review may open.
    pub fn open_refusal(
        &self,
        session_id: &str,
        protocol: u32,
        roots: &[std::path::PathBuf],
    ) -> Option<StartRefusal> {
        if !concurrent_reviews(protocol) {
            return self.owns_any_root(session_id).then(|| {
                StartRefusal(
                    "a review is already open; wait for its verdict or cancel it before starting another"
                        .into(),
                )
            });
        }
        let refusal = match self.first_overlap(session_id, roots)? {
            Overlap::Review { root, review_roots } => {
                let free: Vec<_> = roots
                    .iter()
                    .filter(|candidate| {
                        self.first_overlap(session_id, std::slice::from_ref(candidate))
                            .is_none()
                    })
                    .cloned()
                    .collect();
                let next = if free.is_empty() {
                    format!(
                        "Wait for that verdict and then request {} together.",
                        list(roots)
                    )
                } else {
                    format!(
                        "Request a review of {} now, or wait for that verdict and then request {} together.",
                        list(&free),
                        list(roots)
                    )
                };
                format!(
                    "{} is already in the open review of {}. {next}",
                    root.display(),
                    list(&review_roots)
                )
            }
            Overlap::Operation { label, root } => format!(
                "{} is being changed by the {label} that is still running; request the review again when it finishes.",
                root.as_ref().map_or_else(
                    || "this session's repositories".to_owned(),
                    |root| root.display().to_string()
                )
            ),
        };
        Some(StartRefusal(refusal))
    }

    /// Reserves `roots` for an operation about to change refs, or refuses
    /// because a review, a review being opened or another operation owns one.
    pub fn reserve(
        &mut self,
        operation: u64,
        session_id: &str,
        label: &'static str,
        protocol: u32,
        roots: Vec<std::path::PathBuf>,
        findings: Option<String>,
    ) -> Result<(), StartRefusal> {
        let held = if concurrent_reviews(protocol) {
            if let Some(overlap) = self.first_overlap(session_id, &roots) {
                return Err(StartRefusal(match overlap {
                    Overlap::Review { root, review_roots } => format!(
                        "a review of {} is still open and covers {}; wait for its verdict or cancel it, then try again",
                        list(&review_roots),
                        root.display()
                    ),
                    Overlap::Operation { label, .. } => format!(
                        "the {label} is still running on this session; try again when it finishes"
                    ),
                }));
            }
            ReservedRoots::Roots(roots)
        } else {
            // The worker cannot say which roots the operation touches, so it
            // runs alone, as it always has.
            if self.owns_any_root(session_id)
                || self
                    .preparations
                    .values()
                    .any(|preparation| preparation.session_id == session_id)
            {
                return Err(StartRefusal(
                    "a review is open; wait for its verdict or cancel it, then try again".into(),
                ));
            }
            ReservedRoots::Everything
        };
        let owner = self.review_state.get_mut(session_id).ok_or_else(|| {
            StartRefusal(format!(
                "the review state of session {session_id} is not loaded"
            ))
        })?;
        owner.reservations.push(Reservation {
            operation,
            label,
            roots: held,
            findings,
            retired: None,
        });
        Ok(())
    }

    /// Releases what an operation reserved and returns the reservation.
    pub fn release(&mut self, operation: u64, session_id: &str) -> Option<Reservation> {
        let owner = self.review_state.get_mut(session_id)?;
        let position = owner
            .reservations
            .iter()
            .position(|reservation| reservation.operation == operation)?;
        Some(owner.reservations.remove(position))
    }

    /// The outstanding findings verdict an override answers, selected by the
    /// roots of its `path` or, with no path, by being the only one.
    pub fn select_findings(
        &self,
        session_id: &str,
        protocol: u32,
        roots: Option<&[std::path::PathBuf]>,
    ) -> Result<PushFindingsRecord, StartRefusal> {
        let records = self
            .review_state
            .get(session_id)
            .map(|owner| owner.part.push_findings.as_slice())
            .unwrap_or_default();
        let selected: Vec<&PushFindingsRecord> = match roots {
            Some(roots) => records
                .iter()
                .filter(|record| roots.iter().any(|root| record.covers(root)))
                .collect(),
            None => records.iter().collect(),
        };
        match selected.as_slice() {
            [record] => Ok((*record).clone()),
            [] if records.is_empty() => Err(StartRefusal(
                "no findings verdict is pending; call request_review".into(),
            )),
            [] => Err(StartRefusal(format!(
                "no findings verdict is pending for that path; the outstanding verdicts cover {}",
                list(&outstanding_roots(records))
            ))),
            _ => Err(StartRefusal(format!(
                "more than one findings verdict is outstanding, covering {}; {}",
                list(&outstanding_roots(records)),
                if concurrent_reviews(protocol) {
                    "pass the path of the one to override"
                } else {
                    "restart the session so override_review can name a path"
                }
            ))),
        }
    }
}

fn outstanding_roots(records: &[PushFindingsRecord]) -> Vec<std::path::PathBuf> {
    records
        .iter()
        .flat_map(|record| record.trees.keys().cloned())
        .collect()
}

/// Answers an event that cannot run, when it has a caller waiting.
fn refuse(event: HostEvent, reason: &str) {
    match event {
        HostEvent::StartPush { reply, .. }
        | HostEvent::OverridePush { reply, .. }
        | HostEvent::SetPushBase { reply, .. } => {
            let _ = reply.send(Err(StartRefusal(reason.to_owned())));
        }
        _ => {}
    }
}
