//! Passive status for a review in the parent conversation.
use mj_client::review::RuntimeReviewView;
use mj_core::review::driver::TurnReviewPhase;

#[derive(Debug)]
pub(super) struct TurnReview {
    pub(super) view: RuntimeReviewView,
    failure: Option<String>,
}

impl TurnReview {
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        Self {
            view,
            failure: None,
        }
    }

    pub(super) fn update(&mut self, view: RuntimeReviewView) -> bool {
        let changed = self.view != view;
        self.view = view;
        changed
    }

    pub(super) fn status(&self) -> String {
        self.failure.as_ref().unwrap_or(&self.view.status).clone()
    }

    pub(super) fn pending_elicitations(
        &self,
    ) -> Vec<(String, mj_core::elicitation::ElicitationRequest)> {
        if !matches!(
            self.view.phase,
            TurnReviewPhase::WritingBrief | TurnReviewPhase::Running { .. }
        ) {
            return Vec::new();
        }
        self.view
            .questions
            .iter()
            .filter(|question| self.view.owns_reviewer(&question.role))
            .map(|question| (question.role.clone(), question.request.clone()))
            .collect()
    }

    pub(super) fn report_failure(&mut self, message: impl Into<String>) -> bool {
        let message = message.into();
        if self.failure.as_deref() == Some(message.as_str()) {
            return false;
        }
        self.failure = Some(message);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReviewIntent {
    Start,
}

impl super::ChatState {
    /// The session's open reviews, in the order the daemon published them.
    pub(super) fn turn_reviews(&self) -> &[TurnReview] {
        &self.turn_reviews
    }

    /// The reviews' own views, for the aggregations that read only them.
    pub(super) fn review_views(&self) -> impl Iterator<Item = &RuntimeReviewView> {
        self.turn_reviews.iter().map(|review| &review.view)
    }

    pub(super) fn has_turn_review(&self) -> bool {
        !self.turn_reviews.is_empty()
    }

    /// The one line the footer shows for the reviews: the status of a single
    /// review, or each review's status after its repositories when there are
    /// several.
    pub(super) fn turn_review_status(&self) -> Option<String> {
        match self.turn_reviews.as_slice() {
            [] => None,
            [only] => Some(only.status()),
            several => Some(
                several
                    .iter()
                    .map(|review| {
                        let roots = review.view.roots_label();
                        if roots.is_empty() {
                            review.status()
                        } else {
                            format!("{roots}: {}", review.status())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        }
    }

    /// Reports a refusal on the first review that has none to show, or as a
    /// notice when no review is open.
    pub(super) fn report_turn_review_failure(&mut self, message: String) {
        match self.turn_reviews.first_mut() {
            Some(review) => {
                review.report_failure(message);
            }
            None => self.set_notice(message),
        }
    }

    /// Replaces the open reviews with the daemon's latest projection. A review
    /// that stays open keeps the failure it reported.
    pub(super) fn set_turn_reviews(&mut self, views: Vec<RuntimeReviewView>) {
        self.review_projection_known = true;
        let mut previous = std::mem::take(&mut self.turn_reviews);
        self.turn_reviews = views
            .into_iter()
            .map(|view| {
                match previous
                    .iter()
                    .position(|open| open.view.review_id == view.review_id)
                {
                    Some(index) => {
                        let mut open = previous.swap_remove(index);
                        open.update(view);
                        open
                    }
                    None => TurnReview::new(view),
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: 5fc728617: a writer permission form was hidden until reviewers started.
    #[test]
    fn turn_review_surfaces_writer_questions_and_permissions_while_writing() {
        let mut review = TurnReview::new(serde_json::from_value(serde_json::json!({
            "session_id":"parent",
            "phase":{"phase":"writing_brief"},
            "roles":[{"role":"brief-writer","label":"Brief writer","state":"running","child_session_id":"writer-child"}],
            "status":"writing a relation brief",
            "verdict":null,
            "questions":[
                {"role":"writer-child","request":{"id":"permission","message":"Allow reading the repository?","fields":[],"tool_permission":{"skip":true}}},
                {"role":"writer-child","request":{"id":"question","message":"Which entry point?","fields":[]}},
                {"role":"unrelated-child","request":{"id":"stale","message":"Stale question","fields":[]}}
            ]
        })).unwrap());
        let pending = review.pending_elicitations();
        assert_eq!(
            pending
                .iter()
                .map(|(child, request)| (child.as_str(), request.id.as_str()))
                .collect::<Vec<_>>(),
            [("writer-child", "permission"), ("writer-child", "question")]
        );
        assert!(pending[0].1.tool_permission.is_some());
        review.view.phase = TurnReviewPhase::Running { roles: Vec::new() };
        assert_eq!(review.pending_elicitations(), pending);
        review.view.phase =
            TurnReviewPhase::Resolved(mj_core::review::driver::Resolution::Cancelled);
        assert!(review.pending_elicitations().is_empty());
    }
}
