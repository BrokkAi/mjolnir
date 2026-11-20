use super::*;

impl HostState {
    /// Reattach durable reviewer work without waiting for a worker turn.
    pub fn restore_group(&mut self, session_id: &str, review: OpenReview) -> Result<(), String> {
        let Some(group) = review.active_group.as_ref() else {
            return Err(format!("{session_id}: missing active review group"));
        };
        let snapshot = group.driver.clone();
        if group.reviewers.len() != snapshot.roles.len() {
            return Err(format!(
                "{session_id}: stored review group has inconsistent reviewer slots"
            ));
        }
        let roles: BTreeSet<_> = snapshot
            .roles
            .iter()
            .map(|slot| slot.role.as_str())
            .collect();
        // The brief writer is registered beside the reviewers, never as one.
        let writer = mj_core::review::driver::BRIEF_WRITER_ROLE;
        if group.brief_writer.is_some()
            != group.registrations.iter().any(|item| item.role == writer)
        {
            return Err(format!(
                "{session_id}: stored review group has inconsistent brief writer registration"
            ));
        }
        let registrations: BTreeSet<_> = group
            .registrations
            .iter()
            .map(|item| item.role.as_str())
            .filter(|role| *role != writer)
            .collect();
        let keys: BTreeSet<_> = group
            .registrations
            .iter()
            .map(|item| item.request_key.as_str())
            .collect();
        let children: Vec<_> = group
            .registrations
            .iter()
            .filter_map(|item| item.child_session_id.as_deref())
            .collect();
        let unique_children: BTreeSet<_> = children.iter().copied().collect();
        if roles.len() != snapshot.roles.len()
            || registrations != roles
            || keys.len() != group.registrations.len()
            || group
                .registrations
                .iter()
                .any(|item| item.request_key.is_empty())
            || unique_children.len() != children.len()
            || children.iter().any(|child| child.is_empty())
        {
            return Err(format!(
                "{session_id}: stored review group has inconsistent registrations"
            ));
        }
        let mut driver = TurnReviewDriver::restore(snapshot.clone())
            .map_err(|error| format!("{session_id}: invalid stored review group: {error}"))?;
        let key = ReviewKey {
            session_id: session_id.to_owned(),
            review_id: snapshot.review_id.clone(),
        };
        if self.reviews.contains_key(&key) {
            return Err(format!("{session_id}: review already restored"));
        }
        // Reviews of one session cover disjoint roots. A stored row that says
        // otherwise was not written by this build.
        let roots = review.roots();
        if let Some(Overlap::Review { root, .. }) = self.first_overlap(session_id, &roots) {
            return Err(format!(
                "{session_id}: two stored reviews both cover {}",
                root.display()
            ));
        }
        let requests = driver.recovery_requests();
        self.next_epoch = self.next_epoch.saturating_add(1);
        let reviewers = group.reviewers.clone();
        self.reviews.insert(
            key.clone(),
            ReviewSlot {
                epoch: self.next_epoch,
                driver,
                reviewers,
                open: review,
                // The worker has not reconnected, so its protocol is not
                // known. Restoring assumes the current one.
                protocol: mj_core::relay::RELAY_PROTOCOL_VERSION,
                _background: None,
                finishing: false,
                leaving_row: false,
                delivery: Arc::new(super::resolve::Delivery::default()),
                retry_delay: std::time::Duration::from_secs(1),
                start_reply: None,
                pending_outcome: None,
                queued_outcomes: Vec::new(),
                ended_while_queued: BTreeMap::new(),
            },
        );
        self.run(&key, requests);
        Ok(())
    }
}
