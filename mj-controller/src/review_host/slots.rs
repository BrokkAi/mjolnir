use super::*;

impl ReviewSlot {
    pub(super) fn sync_group(&mut self) {
        if let Some(group) = self.state.active_group.as_mut() {
            group.driver = self.driver.snapshot();
        }
    }

    pub(super) fn view(&self, session_id: &str) -> RuntimeReviewView {
        let verdict = self.driver.last_verdict().map(|v| VerdictView {
            kind: match v {
                ReviewVerdict::Clean => VerdictKind::Clean,
                ReviewVerdict::Findings { .. } => VerdictKind::Findings,
                ReviewVerdict::Failed { .. } => VerdictKind::Failed,
            },
            text: match v {
                ReviewVerdict::Clean => String::new(),
                ReviewVerdict::Findings { synthesis, .. } => synthesis.clone(),
                ReviewVerdict::Failed { reason } => reason.clone(),
            },
            allowed: Vec::new(),
        });
        let mut roles = self.driver.roles();
        if let Some(group) = &self.state.active_group {
            for role in &mut roles {
                role.child_session_id = group
                    .registrations
                    .iter()
                    .find(|item| item.role == role.role)
                    .and_then(|item| item.child_session_id.clone());
            }
        }
        RuntimeReviewView {
            session_id: session_id.to_owned(),
            questions: Vec::new(),
            child_session_id: None,
            phase: self.driver.phase().clone(),
            roles,
            // Matching trees need no reviewer identity to describe.
            status: if self
                .reviewers
                .iter()
                .all(|reviewer| reviewer.profile.is_empty())
            {
                self.driver.status().to_owned()
            } else {
                format!(
                    "{} · {}",
                    self.reviewers
                        .iter()
                        .map(ReviewerIdentity::description)
                        .collect::<Vec<_>>()
                        .join("; "),
                    self.driver.status()
                )
            },
            verdict,
        }
    }
}

/// Where a review start learns whether its review was accepted, with a line
/// for the requester when the review runs with less than it asked for.
pub(super) type StartReply = oneshot::Sender<Result<Option<String>, StartRefusal>>;

pub(super) fn answer(reply: Option<StartReply>, result: Result<(), StartRefusal>) {
    if let Some(reply) = reply {
        let _ = reply.send(result.map(|()| None));
    }
}

/// Accepts the review, telling the requester `note` along with it.
pub(super) fn accept(reply: Option<StartReply>, note: Option<String>) {
    if let Some(reply) = reply {
        let _ = reply.send(Ok(note));
    }
}

pub(super) fn unexpected(outcome: Result<ReviewerOutcome, String>) -> String {
    match outcome {
        Ok(other) => format!("unexpected reviewer response {other:?}"),
        Err(error) => error,
    }
}
