use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use mj_core::relay::RepoDelta;

use super::delta;
use super::lanes::{RepositoryCapture, ReviewJob, review_prompt};
use super::verdict::{ReviewPassEvidence, ReviewVerdict, ReviewerFailure, review_output_verdict};

pub use mj_core::review::driver::*;

/// One turn review, from the capture that starts it to the action that ends it.
#[derive(Debug, Clone)]
pub struct TurnReviewDriver {
    review_id: String,
    seed: TurnReviewSeed,
    phase: TurnReviewPhase,
    /// The verdict that led to the current resolution, retained for the
    /// notice emitted after the resolved phase replaces it.
    last_verdict: Option<ReviewVerdict>,
    deltas: Vec<RepoDelta>,
    /// Who produces this review's findings, which decides how the corrective
    /// prompt describes them.
    /// Frozen launch order and the sole owner of per-role terminal outcomes.
    roles: Vec<ReviewerOutcomeSlot>,
    sequence: u64,
    status: String,
    replay_requests: Vec<ReviewRequest>,
}

impl TurnReviewDriver {
    /// Opens a review and asks for the capture that defines its target.
    #[must_use]
    pub fn start(
        seed: TurnReviewSeed,
        review_id: String,
        count: NonZeroUsize,
    ) -> (Self, Vec<ReviewRequest>) {
        let roles = (1..=count.get())
            .map(|index| ReviewerOutcomeSlot {
                role: format!("reviewer-{index}"),
                command_id: format!("{COMMAND_ID_PREFIX}{review_id}-reviewer-{index}"),
                outcome: None,
            })
            .collect();
        let requests = vec![ReviewRequest::CaptureDelta];
        let driver = Self {
            review_id,
            seed,
            phase: TurnReviewPhase::CapturingDelta,
            last_verdict: None,
            deltas: Vec::new(),
            roles,
            sequence: 0,
            status: "capturing committed work…".to_string(),
            replay_requests: requests.clone(),
        };
        (driver, requests)
    }

    #[must_use]
    pub fn snapshot(&self) -> ReviewDriverSnapshot {
        ReviewDriverSnapshot {
            review_id: self.review_id.clone(),
            seed: self.seed.clone(),
            phase: self.phase.clone(),
            last_verdict: self.last_verdict.clone(),
            deltas: self.deltas.clone(),
            roles: self.roles.clone(),
            sequence: self.sequence,
            replay_requests: self.replay_requests.clone(),
        }
    }

    /// Replay the exact work this stage emitted before a daemon interruption.
    /// A settled role is never restarted while its peers are still running.
    pub fn recovery_requests(&mut self) -> Vec<ReviewRequest> {
        match self.phase {
            TurnReviewPhase::CapturingDelta => self
                .replay_requests
                .iter()
                .filter(|request| matches!(request, ReviewRequest::CaptureDelta))
                .cloned()
                .collect(),
            TurnReviewPhase::Running { .. } => self
                .replay_requests
                .iter()
                .filter(|request| match request {
                    ReviewRequest::StartReviewer { role, .. } => self
                        .roles
                        .iter()
                        .any(|slot| &slot.role == role && slot.outcome.is_none()),
                    _ => false,
                })
                .cloned()
                .collect(),
            TurnReviewPhase::Verdict(_) | TurnReviewPhase::Resolved(_) => {
                self.replay_requests.clone()
            }
            TurnReviewPhase::LaunchingReviewer => Vec::new(),
        }
    }

    fn emit(&mut self, requests: Vec<ReviewRequest>) -> Vec<ReviewRequest> {
        self.replay_requests = requests.clone();
        requests
    }

    pub fn restore(snapshot: ReviewDriverSnapshot) -> Result<Self, String> {
        if snapshot.review_id.is_empty() {
            return Err("review snapshot has no review id".into());
        }
        let mut names = BTreeSet::new();
        let mut commands = BTreeSet::new();
        for slot in &snapshot.roles {
            if slot.role.is_empty()
                || slot.command_id.is_empty()
                || !names.insert(&slot.role)
                || !commands.insert(&slot.command_id)
            {
                return Err("review snapshot has duplicate or empty role/command ids".into());
            }
        }
        if snapshot.roles.is_empty() {
            return Err("review snapshot has no reviewer slots".into());
        }
        if matches!(snapshot.phase, TurnReviewPhase::Running { .. })
            && snapshot.roles.iter().all(|slot| slot.outcome.is_some())
        {
            return Err("running review snapshot has no pending reviewer".into());
        }
        if matches!(snapshot.phase, TurnReviewPhase::CapturingDelta)
            && (snapshot.roles.iter().any(|slot| slot.outcome.is_some())
                || !snapshot.deltas.is_empty())
        {
            return Err("capturing review snapshot already has outcomes or a target".into());
        }
        if let TurnReviewPhase::Verdict(verdict) = &snapshot.phase
            && (snapshot.last_verdict.as_ref() != Some(verdict)
                || snapshot.roles.iter().any(|slot| slot.outcome.is_none()))
        {
            return Err("review snapshot verdict disagrees with its completed slots".into());
        }
        let mut driver = Self {
            review_id: snapshot.review_id,
            seed: snapshot.seed,
            phase: snapshot.phase,
            last_verdict: snapshot.last_verdict,
            deltas: snapshot.deltas,
            roles: snapshot.roles,
            sequence: snapshot.sequence,
            status: String::new(),
            replay_requests: snapshot.replay_requests,
        };
        if matches!(driver.phase, TurnReviewPhase::Running { .. }) {
            driver.phase = TurnReviewPhase::Running {
                roles: driver.role_statuses(),
            };
        }
        driver.status = driver.phase_status();
        Ok(driver)
    }

    #[must_use]
    pub fn phase(&self) -> &TurnReviewPhase {
        &self.phase
    }

    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The command each running role is answering, for a caller matching the
    /// relay's completion records. The newest message in a role's pane is not
    /// enough on its own: an earlier prompt's answer can still be the newest
    /// message in a role's journal when its next command starts.
    #[must_use]
    pub fn awaited_commands(&self) -> Vec<(String, String)> {
        if !matches!(self.phase, TurnReviewPhase::Running { .. }) {
            return Vec::new();
        }
        self.roles
            .iter()
            .filter(|slot| slot.outcome.is_none())
            .map(|slot| (slot.role.clone(), slot.command_id.clone()))
            .collect()
    }

    /// Whether the review has ended, which is when its pane closes and the
    /// composer comes back.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.phase, TurnReviewPhase::Resolved(_))
    }

    /// Whether a verdict is on screen and the user has actions to take.
    #[must_use]
    pub fn verdict(&self) -> Option<&ReviewVerdict> {
        match &self.phase {
            TurnReviewPhase::Verdict(verdict) => Some(verdict),
            _ => None,
        }
    }

    /// The last verdict reached, including after the review has resolved.
    #[must_use]
    pub fn last_verdict(&self) -> Option<&ReviewVerdict> {
        self.last_verdict.as_ref()
    }

    pub fn deltas(&self) -> &[RepoDelta] {
        &self.deltas
    }

    /// The rows the review pane's strip shows.
    #[must_use]
    pub fn roles(&self) -> Vec<RoleStatus> {
        match &self.phase {
            TurnReviewPhase::Running { .. } | TurnReviewPhase::Verdict(_) => self.role_statuses(),
            _ => Vec::new(),
        }
    }

    fn role_statuses(&self) -> Vec<RoleStatus> {
        self.roles
            .iter()
            .enumerate()
            .map(|(index, slot)| RoleStatus {
                role: slot.role.clone(),
                label: format!("Reviewer {}", index + 1),
                state: match &slot.outcome {
                    Some(ReviewVerdict::Clean) => RoleState::Clean,
                    Some(ReviewVerdict::Findings { .. }) => RoleState::Findings,
                    Some(ReviewVerdict::Failed { .. }) => RoleState::Failed,
                    None if matches!(self.phase, TurnReviewPhase::CapturingDelta) => {
                        RoleState::Pending
                    }
                    None => RoleState::Running,
                },
                child_session_id: None,
            })
            .collect()
    }

    fn phase_status(&self) -> String {
        match &self.phase {
            TurnReviewPhase::CapturingDelta => "capturing committed work…".into(),
            TurnReviewPhase::LaunchingReviewer => "starting reviewers…".into(),
            TurnReviewPhase::Running { .. } => "reviewers are reading the change…".into(),
            TurnReviewPhase::Verdict(ReviewVerdict::Clean) => "no material findings".into(),
            TurnReviewPhase::Verdict(ReviewVerdict::Findings { .. }) => {
                "delivering review verdict".into()
            }
            TurnReviewPhase::Verdict(ReviewVerdict::Failed { reason }) => {
                format!("the review failed: {reason}")
            }
            TurnReviewPhase::Resolved(Resolution::Cancelled) => "review cancelled".into(),
            TurnReviewPhase::Resolved(_) => "review complete".into(),
        }
    }

    /// The reviewer prompt for a reviewer that reads `checkout` instead of the
    /// captured repository's own worktree. Only a review of one repository has
    /// a checkout to name.
    #[must_use]
    pub fn review_prompt_in(&self, checkout: &std::path::Path) -> String {
        review_prompt(&self.job_in(Some(checkout)))
    }

    fn job(&self) -> ReviewJob {
        self.job_in(None)
    }

    fn job_in(&self, checkout: Option<&std::path::Path>) -> ReviewJob {
        ReviewJob {
            task: self.seed.task.clone(),
            user_messages: self.seed.user_messages.clone(),
            repositories: self
                .deltas
                .iter()
                .filter(|delta| !delta.patch.trim().is_empty())
                .filter_map(|delta| {
                    delta
                        .baseline_tree
                        .as_ref()
                        .map(|baseline_tree| RepositoryCapture {
                            root: delta.root.clone(),
                            baseline_tree: baseline_tree.clone(),
                            capture_tree: delta.current_tree.clone(),
                            merged_in: delta.merged_in.clone(),
                            unattributed: delta.unattributed.clone(),
                            checkout: checkout.map(std::path::Path::to_path_buf),
                        })
                })
                .collect(),
            changed_files: delta::changed_files_table_in(&self.deltas, checkout),
            changed_lines: delta::changed_line_count(&self.deltas),
            declined: self.seed.declined.clone(),
        }
    }

    /// The capture landed. An empty capture ends the review before any agent
    /// runs; anything else starts the first agent.
    pub fn delta_captured(&mut self, deltas: Vec<RepoDelta>) -> Vec<ReviewRequest> {
        if !matches!(self.phase, TurnReviewPhase::CapturingDelta) {
            return Vec::new();
        }
        self.deltas = deltas;
        let prompt = review_prompt(&self.job());
        self.phase = TurnReviewPhase::Running { roles: Vec::new() };
        self.phase = TurnReviewPhase::Running {
            roles: self.role_statuses(),
        };
        self.status = self.phase_status();
        let requests = self
            .roles
            .iter()
            .map(|slot| ReviewRequest::StartReviewer {
                role: slot.role.clone(),
                command_id: slot.command_id.clone(),
                prompt: prompt.clone(),
            })
            .collect();
        self.emit(requests)
    }

    /// A push whose tree matches its reviewed base needs no reviewer. The
    /// controller has already refused a capture with base == HEAD.
    pub fn empty_push_delta_captured(&mut self, deltas: Vec<RepoDelta>) -> Vec<ReviewRequest> {
        if !matches!(self.phase, TurnReviewPhase::CapturingDelta) {
            return Vec::new();
        }
        self.deltas = deltas;
        self.reach_verdict(ReviewVerdict::Clean)
    }

    pub fn push_review_warnings(&self) -> impl Iterator<Item = &str> {
        self.deltas
            .iter()
            .filter_map(|delta| delta.push_review_warning.as_deref())
    }

    /// A role finished its turn.
    pub fn role_turn_completed(
        &mut self,
        command_id: &str,
        child_session_id: &str,
        answer: &str,
    ) -> Vec<ReviewRequest> {
        if !matches!(self.phase, TurnReviewPhase::Running { .. }) {
            return Vec::new();
        }
        let Some(index) = self
            .roles
            .iter()
            .position(|slot| slot.command_id == command_id && slot.outcome.is_none())
        else {
            return Vec::new();
        };
        let label = self.role_label(index, Some(child_session_id));
        let outcome = match review_output_verdict(answer) {
            ReviewVerdict::Findings {
                synthesis,
                evidence,
            } => ReviewVerdict::Findings {
                synthesis: format!("{label}\n{synthesis}"),
                evidence,
            },
            ReviewVerdict::Failed { reason } => ReviewVerdict::Failed {
                reason: format!("{label}: {reason}"),
            },
            other => other,
        };
        self.roles[index].outcome = Some(outcome);
        self.role_settled()
    }

    /// Records an explicit lifecycle or harness failure of one role.
    pub fn role_failed(
        &mut self,
        role: &str,
        child_session_id: Option<&str>,
        reason: impl Into<String>,
    ) -> Vec<ReviewRequest> {
        if !matches!(self.phase, TurnReviewPhase::Running { .. }) {
            return Vec::new();
        }
        let Some(index) = self
            .roles
            .iter()
            .position(|slot| slot.role == role && slot.outcome.is_none())
        else {
            return Vec::new();
        };
        let label = self.role_label(index, child_session_id);
        self.roles[index].outcome = Some(ReviewVerdict::Failed {
            reason: format!("{label}: {}", reason.into()),
        });
        self.role_settled()
    }

    fn role_label(&self, index: usize, child_session_id: Option<&str>) -> String {
        match child_session_id {
            Some(id) => format!("Reviewer {} (session {id})", index + 1),
            None => format!("Reviewer {}", index + 1),
        }
    }

    fn role_settled(&mut self) -> Vec<ReviewRequest> {
        self.phase = TurnReviewPhase::Running {
            roles: self.role_statuses(),
        };
        if self.roles.iter().any(|slot| slot.outcome.is_none()) {
            return Vec::new();
        }
        let mut sections = Vec::new();
        let mut failures = Vec::new();
        for slot in &self.roles {
            match slot.outcome.as_ref().expect("all slots settled") {
                ReviewVerdict::Clean => {}
                ReviewVerdict::Findings { synthesis, .. } => sections.push(synthesis.clone()),
                ReviewVerdict::Failed { reason } => {
                    let (label, detail) = reason
                        .split_once(": ")
                        .unwrap_or((&slot.role, reason.as_str()));
                    sections.push(format!("{label}\nReview failed: {detail}"));
                    failures.push(ReviewerFailure {
                        label: label.to_owned(),
                        reason: detail.to_owned(),
                    });
                }
            }
        }
        let has_findings = self
            .roles
            .iter()
            .any(|slot| matches!(slot.outcome.as_ref(), Some(ReviewVerdict::Findings { .. })));
        let verdict = if has_findings {
            let synthesis = sections.join("\n\n");
            ReviewVerdict::Findings {
                synthesis: if self.roles.len() == 1 {
                    synthesis
                } else {
                    format!(
                        "Independent reviewer reports may overlap. Check each finding against the source.\n\n{synthesis}"
                    )
                },
                evidence: ReviewPassEvidence {
                    reviewer_failures: failures,
                    ..Default::default()
                },
            }
        } else if failures.is_empty() {
            ReviewVerdict::Clean
        } else {
            ReviewVerdict::Failed {
                reason: failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.label, failure.reason))
                    .collect::<Vec<_>>()
                    .join("; "),
            }
        };
        self.reach_verdict(verdict)
    }

    /// A group-wide request, such as capture, failed before role results can
    /// decide the verdict. Role-specific failures use `role_failed` instead.
    pub fn request_failed(&mut self, message: impl Into<String>) -> Vec<ReviewRequest> {
        if self.finished() {
            return Vec::new();
        }
        self.reach_verdict(ReviewVerdict::Failed {
            reason: message.into(),
        })
    }

    fn reach_verdict(&mut self, verdict: ReviewVerdict) -> Vec<ReviewRequest> {
        // Empty captures and group-wide failures finish without individual
        // reports. Persist their terminal outcomes before delivery can race
        // daemon replacement.
        for slot in &mut self.roles {
            if slot.outcome.is_none() {
                slot.outcome = Some(verdict.clone());
            }
        }
        self.status = match &verdict {
            ReviewVerdict::Clean => "no material findings".to_string(),
            ReviewVerdict::Findings { .. } => "delivering review verdict".to_string(),
            ReviewVerdict::Failed { reason } => format!("the review failed: {reason}"),
        };
        self.last_verdict = Some(verdict.clone());
        self.phase = TurnReviewPhase::Verdict(verdict);
        // Every role is reaped before the review resolves, whichever way it
        // resolves: a reviewing harness must never outlive the review.
        let requests = self.pause_every_role();
        self.emit(requests)
    }

    fn pause_every_role(&mut self) -> Vec<ReviewRequest> {
        if self.deltas.is_empty() {
            return Vec::new();
        }
        self.roles
            .iter()
            .map(|slot| ReviewRequest::CloseReviewer {
                role: slot.role.clone(),
            })
            .collect()
    }

    /// Stops the review. The baseline stays where it was, so the next review
    /// covers this turn as well as the next one.
    pub fn complete(&mut self) {
        self.phase = TurnReviewPhase::Resolved(Resolution::Completed);
    }

    pub fn cancel(&mut self) -> Vec<ReviewRequest> {
        if self.finished() {
            return Vec::new();
        }
        let mut requests = self.pause_every_role();
        self.phase = TurnReviewPhase::Resolved(Resolution::Cancelled);
        self.status = "review cancelled".to_string();
        requests.push(ReviewRequest::Close);
        self.emit(requests)
    }
}

#[cfg(test)]
mod tests;
