use super::*;

impl ReviewSlot {
    /// The roots the review owns until it is dropped. They come from the
    /// capture, which outlives the stored group, so closing a review does not
    /// release its roots before the host counts the close.
    pub fn roots(&self) -> Vec<std::path::PathBuf> {
        self.driver
            .deltas()
            .iter()
            .map(|delta| delta.root.clone())
            .collect()
    }

    pub fn sync_group(&mut self) {
        if let Some(group) = self.open.active_group.as_mut() {
            group.driver = self.driver.snapshot();
        }
    }

    pub fn view(&self, session_id: &str) -> RuntimeReviewView {
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
        if let Some(group) = &self.open.active_group {
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
            review_id: self.driver.snapshot().review_id,
            roots: self
                .roots()
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
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

/// Tells the caller of `request_review` how its review went: what was
/// accepted, or why not.
pub fn answer(reply: Option<OpenReply>, result: Result<(), StartRefusal>) {
    if let Some(OpenReply { reply, accepted }) = reply {
        let _ = reply.send(result.map(|()| accepted));
    }
}

pub fn unexpected(outcome: Result<ReviewerOutcome, String>) -> String {
    match outcome {
        Ok(other) => format!("unexpected reviewer response {other:?}"),
        Err(error) => error,
    }
}
