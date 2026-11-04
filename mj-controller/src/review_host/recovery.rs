use super::*;

impl HostState {
    /// Reattach durable reviewer work without waiting for a worker turn.
    pub(super) fn restore_group(
        &mut self,
        session_id: String,
        state: TurnReviewState,
    ) -> Result<(), String> {
        let Some(group) = state.active_group.as_ref() else {
            return Err(format!("{session_id}: missing active review group"));
        };
        let snapshot = group.driver.clone();
        let roles: BTreeSet<_> = snapshot
            .roles
            .iter()
            .map(|slot| slot.role.as_str())
            .collect();
        let registrations: BTreeSet<_> = group
            .registrations
            .iter()
            .map(|item| item.role.as_str())
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
        if self.reviews.contains_key(&session_id) {
            return Err(format!("{session_id}: review already restored"));
        }
        let requests = driver.recovery_requests();
        self.next_epoch = self.next_epoch.saturating_add(1);
        let push =
            (group.scope == mj_core::relay::CaptureScope::PushableCommits).then(|| PushReview {
                deltas: snapshot.deltas,
                finishing: false,
                retry_delay: std::time::Duration::from_secs(1),
            });
        if push.is_none() && state.active.is_some() {
            hold_prompts(&session_id);
        }
        self.reviews.insert(
            session_id.clone(),
            ReviewSlot {
                epoch: self.next_epoch,
                driver,
                reviewer: group.reviewer.clone(),
                state,
                _background: None,
                push,
                start_reply: None,
                pending_outcome: None,
                queued_outcomes: Vec::new(),
            },
        );
        self.run(&session_id, requests);
        Ok(())
    }
}
