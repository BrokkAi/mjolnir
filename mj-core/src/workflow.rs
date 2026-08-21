//! Runtime-owned workflow identity and monotonic state reduction.
//!
//! Human-readable transcript messages are evidence and diagnostics. They never
//! drive this state: orchestration code emits typed transitions from facts it
//! owns, and every consumer applies the same reducer.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::event::{SubagentOutcome, UiEvent};

const MAX_RETAINED_WORKFLOWS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkflowId {
    pub turn_id: u64,
    pub operation: u32,
}

impl WorkflowId {
    pub const fn review(turn_id: u64) -> Self {
        Self {
            turn_id,
            operation: 1,
        }
    }

    /// An explicit review is not tied to the preceding completed turn's
    /// automatic review workflow, so it keeps a distinct operation identity.
    pub const fn manual_review(turn_id: u64) -> Self {
        Self {
            turn_id,
            operation: 2,
        }
    }

    pub const fn delegation(turn_id: u64) -> Self {
        Self {
            turn_id,
            operation: 0,
        }
    }
}

impl fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn-{}-workflow-{}", self.turn_id, self.operation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowKind {
    Delegation,
    Review,
}

impl WorkflowKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delegation => "delegation",
            Self::Review => "review",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkflowPhase {
    Delegating,
    IntentAnalysis,
    Supervision,
    SpecialistReview,
    Synthesis,
    Correction,
    Fallback,
    Terminal,
}

impl WorkflowPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delegating => "delegating",
            Self::IntentAnalysis => "intent_analysis",
            Self::Supervision => "supervision",
            Self::SpecialistReview => "specialist_review",
            Self::Synthesis => "synthesis",
            Self::Correction => "correction",
            Self::Fallback => "fallback",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkflowStage {
    pub pass: u32,
    pub phase: WorkflowPhase,
}

impl WorkflowStage {
    pub const fn new(pass: u32, phase: WorkflowPhase) -> Self {
        Self { pass, phase }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkflowActorId {
    Subagent(u64),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowActorRole {
    Implementation,
    IntentAnalyst,
    ReviewSupervisor,
    SpecialistReviewer { lane: String },
    PrimaryCorrection,
    FallbackReviewer,
}

impl WorkflowActorRole {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::IntentAnalyst => "intent_analyst",
            Self::ReviewSupervisor => "review_supervisor",
            Self::SpecialistReviewer { .. } => "specialist_reviewer",
            Self::PrimaryCorrection => "primary_correction",
            Self::FallbackReviewer => "fallback_reviewer",
        }
    }

    /// Internal review coordinators use the nested-agent runtime for process
    /// isolation, but they are not user-delegated subagents and must not be
    /// presented or counted as such.
    pub const fn is_internal_review_session(&self) -> bool {
        matches!(self, Self::IntentAnalyst | Self::ReviewSupervisor)
    }

    pub const fn display_label(&self) -> &'static str {
        match self {
            Self::Implementation => "subagent",
            Self::IntentAnalyst => "review intent",
            Self::ReviewSupervisor => "review supervisor",
            Self::SpecialistReviewer { .. } => "reviewer",
            Self::PrimaryCorrection => "primary correction",
            Self::FallbackReviewer => "fallback reviewer",
        }
    }

    pub const fn actor_prefix(&self) -> &'static str {
        match self {
            Self::IntentAnalyst => "review-intent",
            Self::ReviewSupervisor => "review-supervisor",
            Self::Implementation
            | Self::SpecialistReviewer { .. }
            | Self::PrimaryCorrection
            | Self::FallbackReviewer => "subagent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowActorLifecycle {
    Running,
    Waiting {
        dependency: String,
        remaining: Option<usize>,
        requires_user_action: bool,
    },
    Completed,
    Failed(String),
    Cancelled,
}

impl WorkflowActorLifecycle {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting { .. } => "waiting",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_) | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowActorState {
    pub role: WorkflowActorRole,
    pub lifecycle: WorkflowActorLifecycle,
    pub retained_session_id: Option<String>,
    pub resume_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowWait {
    pub dependency: String,
    pub remaining: Option<usize>,
    pub requires_user_action: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowCoverage {
    Unknown,
    Complete,
    Degraded,
}

impl WorkflowCoverage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Complete => "complete",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOutcome {
    Completed,
    Clean,
    Degraded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewIssueStatus {
    /// The review supervisor confirmed the finding, but no correction has
    /// completed for it yet.
    Validated,
    /// A primary correction changed the workspace. The change is retained as
    /// evidence, but a later verification review has not cleared it yet.
    Corrected,
    /// A verification review completed clean after the correction.
    Fixed,
    /// The finding survived review but was below the configured automatic
    /// correction threshold. It remains tracked, with the threshold recorded
    /// as its explicit reason for not being sent to the primary.
    Deferred,
    /// The primary correction completed without changing the workspace, so
    /// this validated finding remains unresolved.
    Uncorrected,
    /// Retained for explicit review invalidations. A no-op correction is not
    /// enough evidence to put a finding in this state.
    Invalidated,
}

impl ReviewIssueStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Validated => "validated",
            Self::Corrected => "corrected; verification pending",
            Self::Fixed => "verified fixed",
            Self::Deferred => "deferred by correction threshold",
            Self::Uncorrected => "unresolved",
            Self::Invalidated => "invalidated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewIssue {
    pub id: usize,
    pub pass: u32,
    pub summary: String,
    pub status: ReviewIssueStatus,
    /// Concise outcome of the correction or verification. `None` while the
    /// issue is still awaiting a correction.
    pub resolution_reason: Option<String>,
    /// Full primary correction report and captured corrective diff. Kept
    /// separately from the concise reason so compact rows stay readable while
    /// the F9 ledger can show the complete evidence.
    pub resolution_details: Option<String>,
}

/// Per-status totals across a workflow's review issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReviewIssueTally {
    pub found: usize,
    pub open: usize,
    pub corrected: usize,
    pub fixed: usize,
    pub deferred: usize,
    pub uncorrected: usize,
    pub invalidated: usize,
}

impl ReviewIssueTally {
    pub fn count(issues: &[ReviewIssue]) -> Self {
        let mut tally = Self {
            found: issues.len(),
            ..Self::default()
        };
        for issue in issues {
            match issue.status {
                ReviewIssueStatus::Validated => tally.open += 1,
                ReviewIssueStatus::Corrected => tally.corrected += 1,
                ReviewIssueStatus::Fixed => tally.fixed += 1,
                ReviewIssueStatus::Deferred => tally.deferred += 1,
                ReviewIssueStatus::Uncorrected => tally.uncorrected += 1,
                ReviewIssueStatus::Invalidated => tally.invalidated += 1,
            }
        }
        tally
    }
}

impl WorkflowOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Clean => "clean",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowState {
    pub id: WorkflowId,
    pub kind: WorkflowKind,
    pub stage: WorkflowStage,
    pub actors: BTreeMap<WorkflowActorId, WorkflowActorState>,
    pub waiting: Option<WorkflowWait>,
    pub coverage: WorkflowCoverage,
    pub outcome: Option<WorkflowOutcome>,
    pub issues: Vec<ReviewIssue>,
}

impl WorkflowState {
    pub fn issue_tally(&self) -> ReviewIssueTally {
        ReviewIssueTally::count(&self.issues)
    }

    pub fn selected_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.role, WorkflowActorRole::SpecialistReviewer { .. }))
            .count()
    }

    pub fn running_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.lifecycle, WorkflowActorLifecycle::Running))
            .count()
    }

    pub fn waiting_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.lifecycle, WorkflowActorLifecycle::Waiting { .. }))
            .count()
    }

    pub fn completed_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.lifecycle, WorkflowActorLifecycle::Completed))
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.lifecycle, WorkflowActorLifecycle::Failed(_)))
            .count()
    }

    pub fn cancelled_count(&self) -> usize {
        self.actors
            .values()
            .filter(|actor| matches!(actor.lifecycle, WorkflowActorLifecycle::Cancelled))
            .count()
    }

    fn unfinished_count(&self) -> usize {
        self.running_count() + self.waiting_count()
    }

    /// Transcript line for this workflow starting, or `None` when the start is
    /// not worth a permanent entry.
    ///
    /// The three `*_notice` renderers below live here rather than in either
    /// consumer because the TUI and the remote mirror both fold workflow
    /// lifecycle into a transcript. Duplicating the wording is how the two
    /// transcripts drift apart.
    pub fn started_notice(&self) -> Option<String> {
        (self.kind == WorkflowKind::Review).then(|| "review started".to_string())
    }

    /// Transcript line for the workflow blocking on its actors.
    pub fn waiting_notice(
        &self,
        remaining: Option<usize>,
        requires_user_action: bool,
    ) -> Option<String> {
        if self.kind != WorkflowKind::Review {
            return None;
        }
        if requires_user_action {
            return Some("review · waiting for user action".to_string());
        }
        let selected = self.selected_count();
        Some(match remaining {
            Some(remaining) if selected > 0 => {
                format!("review · waiting for {remaining} of {selected} selected reviewers")
            }
            Some(remaining) => format!("review · waiting for {remaining} reviewers"),
            None => "review · waiting for reviewer reports".to_string(),
        })
    }

    /// Transcript line summarising how this workflow ended.
    pub fn terminal_notice(&self, outcome: WorkflowOutcome) -> String {
        match self.kind {
            WorkflowKind::Review => {
                let head = match outcome {
                    WorkflowOutcome::Clean if self.issues.is_empty() => {
                        "review complete · no material findings"
                    }
                    WorkflowOutcome::Clean | WorkflowOutcome::Completed => "review complete",
                    WorkflowOutcome::Degraded => "review complete · degraded coverage",
                    WorkflowOutcome::Failed => "review failed",
                    WorkflowOutcome::Cancelled => "review cancelled",
                };
                if self.issues.is_empty() {
                    return head.to_string();
                }
                // The final tally is the record the user scrolls back for;
                // "review complete" alone buries the verdict.
                let tally = self.issue_tally();
                let mut parts = vec![
                    head.to_string(),
                    format!(
                        "{} issue{}",
                        tally.found,
                        if tally.found == 1 { "" } else { "s" }
                    ),
                ];
                for (count, label) in [
                    (tally.fixed, "verified fixed"),
                    (tally.corrected, "corrected; unverified"),
                    (tally.deferred, "deferred by policy"),
                    (tally.uncorrected, "unresolved"),
                    (tally.invalidated, "invalidated"),
                    (tally.open, "awaiting correction"),
                ] {
                    if count > 0 {
                        parts.push(format!("{count} {label}"));
                    }
                }
                parts.join(" · ")
            }
            WorkflowKind::Delegation => {
                let head = match outcome {
                    WorkflowOutcome::Completed
                    | WorkflowOutcome::Clean
                    | WorkflowOutcome::Degraded => "subagents complete",
                    WorkflowOutcome::Failed => "subagents failed",
                    WorkflowOutcome::Cancelled => "subagents cancelled",
                };
                let mut parts = vec![head.to_string()];
                for (count, label) in [
                    (self.completed_count(), "completed"),
                    (self.failed_count(), "failed"),
                    (self.cancelled_count(), "cancelled"),
                ] {
                    if count > 0 {
                        parts.push(format!("{count} {label}"));
                    }
                }
                if self.coverage == WorkflowCoverage::Degraded {
                    parts.push("degraded coverage".to_string());
                }
                parts.join(" · ")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEvent {
    pub workflow_id: WorkflowId,
    pub transition: WorkflowTransition,
}

impl WorkflowEvent {
    pub fn new(workflow_id: WorkflowId, transition: WorkflowTransition) -> Self {
        Self {
            workflow_id,
            transition,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowTransition {
    Started {
        kind: WorkflowKind,
        stage: WorkflowStage,
    },
    PhaseChanged {
        stage: WorkflowStage,
    },
    ActorStarted {
        actor_id: WorkflowActorId,
        role: WorkflowActorRole,
    },
    ActorSessionBound {
        actor_id: WorkflowActorId,
        retained_session_id: String,
    },
    ActorWaiting {
        actor_id: WorkflowActorId,
        dependency: String,
        remaining: Option<usize>,
        requires_user_action: bool,
    },
    ActorResumed {
        actor_id: WorkflowActorId,
    },
    ActorFinished {
        actor_id: WorkflowActorId,
        outcome: SubagentOutcome,
    },
    Waiting {
        dependency: String,
        remaining: Option<usize>,
        requires_user_action: bool,
    },
    CoverageChanged {
        coverage: WorkflowCoverage,
    },
    IssuesValidated {
        pass: u32,
        summaries: Vec<String>,
    },
    IssuesResolved {
        pass: u32,
        /// When present, update only these exact finding summaries in the
        /// pass. This lets a mixed-priority verdict defer P2/P3 findings while
        /// still sending P0/P1 findings through correction.
        summaries: Option<Vec<String>>,
        status: ReviewIssueStatus,
        /// Concise outcome for compact rows, for example that a correction
        /// changed no files or that a verification pass completed clean.
        reason: Option<String>,
        /// Full correction evidence for the F9 issue reader. This includes the
        /// primary's report and the exact captured correction patch when one
        /// exists; compact transcript rows intentionally omit it.
        details: Option<String>,
    },
    /// Refreshes the evidence for a correction that is already recorded as
    /// `Corrected`. This preserves the live board state without appending a
    /// second resolution record when the primary finally ends its turn.
    IssueEvidenceUpdated {
        pass: u32,
        /// Limit the evidence refresh to the exact findings that were sent to
        /// the primary. Deferred findings in a mixed-priority pass remain
        /// deferred rather than being treated as part of that correction.
        summaries: Option<Vec<String>>,
        reason: String,
        details: String,
    },
    Terminal {
        outcome: WorkflowOutcome,
        coverage: WorkflowCoverage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTransitionError {
    pub workflow_id: WorkflowId,
    pub message: String,
}

impl fmt::Display for WorkflowTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.workflow_id, self.message)
    }
}

impl std::error::Error for WorkflowTransitionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Changed,
    Duplicate,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowStore {
    states: BTreeMap<WorkflowId, WorkflowState>,
}

impl WorkflowStore {
    pub fn get(&self, id: WorkflowId) -> Option<&WorkflowState> {
        self.states.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &WorkflowState> {
        self.states.values()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.states.len()
    }

    pub fn apply(
        &mut self,
        event: &WorkflowEvent,
    ) -> Result<ApplyOutcome, WorkflowTransitionError> {
        if let WorkflowTransition::Started { kind, stage } = event.transition {
            if let Some(existing) = self.states.get(&event.workflow_id) {
                return if existing.kind == kind && existing.stage == stage {
                    Ok(ApplyOutcome::Duplicate)
                } else {
                    Err(Self::error(
                        event.workflow_id,
                        "workflow start conflicts with existing state",
                    ))
                };
            }
            self.make_room();
            self.states.insert(
                event.workflow_id,
                WorkflowState {
                    id: event.workflow_id,
                    kind,
                    stage,
                    actors: BTreeMap::new(),
                    waiting: None,
                    coverage: WorkflowCoverage::Unknown,
                    outcome: None,
                    issues: Vec::new(),
                },
            );
            return Ok(ApplyOutcome::Changed);
        }

        let Some(state) = self.states.get_mut(&event.workflow_id) else {
            return Err(Self::error(event.workflow_id, "workflow has not started"));
        };
        if state.outcome.is_some() {
            return match &event.transition {
                WorkflowTransition::Terminal { outcome, coverage }
                    if state.outcome == Some(*outcome) && state.coverage == *coverage =>
                {
                    Ok(ApplyOutcome::Duplicate)
                }
                _ => Err(Self::error(
                    event.workflow_id,
                    "workflow is already terminal",
                )),
            };
        }

        match &event.transition {
            WorkflowTransition::Started { .. } => unreachable!(),
            WorkflowTransition::PhaseChanged { stage } => {
                if *stage < state.stage {
                    return Err(Self::error(
                        event.workflow_id,
                        "workflow phase would regress",
                    ));
                }
                if *stage == state.stage {
                    return Ok(ApplyOutcome::Duplicate);
                }
                state.stage = *stage;
                state.waiting = None;
            }
            WorkflowTransition::ActorStarted { actor_id, role } => {
                if let Some(existing) = state.actors.get(actor_id) {
                    return if existing.role == *role
                        && matches!(existing.lifecycle, WorkflowActorLifecycle::Running)
                    {
                        Ok(ApplyOutcome::Duplicate)
                    } else {
                        Err(Self::error(
                            event.workflow_id,
                            "actor start conflicts with existing actor state",
                        ))
                    };
                }
                state.actors.insert(
                    actor_id.clone(),
                    WorkflowActorState {
                        role: role.clone(),
                        lifecycle: WorkflowActorLifecycle::Running,
                        retained_session_id: None,
                        resume_count: 0,
                    },
                );
            }
            WorkflowTransition::ActorSessionBound {
                actor_id,
                retained_session_id,
            } => {
                let Some(actor) = state.actors.get_mut(actor_id) else {
                    return Err(Self::error(
                        event.workflow_id,
                        "cannot bind an unknown actor",
                    ));
                };
                match actor.retained_session_id.as_ref() {
                    None => actor.retained_session_id = Some(retained_session_id.clone()),
                    Some(existing) if existing == retained_session_id => {
                        return Ok(ApplyOutcome::Duplicate);
                    }
                    Some(_) => {
                        return Err(Self::error(
                            event.workflow_id,
                            "actor session identity cannot change",
                        ));
                    }
                }
            }
            WorkflowTransition::ActorWaiting {
                actor_id,
                dependency,
                remaining,
                requires_user_action,
            } => {
                if dependency.trim().is_empty() {
                    return Err(Self::error(
                        event.workflow_id,
                        "waiting dependency is empty",
                    ));
                }
                let Some(actor) = state.actors.get_mut(actor_id) else {
                    return Err(Self::error(
                        event.workflow_id,
                        "cannot wait an unknown actor",
                    ));
                };
                let waiting = WorkflowActorLifecycle::Waiting {
                    dependency: dependency.clone(),
                    remaining: *remaining,
                    requires_user_action: *requires_user_action,
                };
                if actor.lifecycle == waiting {
                    return Ok(ApplyOutcome::Duplicate);
                }
                if actor.lifecycle.is_terminal() {
                    return Err(Self::error(event.workflow_id, "terminal actor cannot wait"));
                }
                actor.lifecycle = waiting;
            }
            WorkflowTransition::ActorResumed { actor_id } => {
                let Some(actor) = state.actors.get_mut(actor_id) else {
                    return Err(Self::error(
                        event.workflow_id,
                        "cannot resume an unknown actor",
                    ));
                };
                if matches!(actor.lifecycle, WorkflowActorLifecycle::Running) {
                    return Ok(ApplyOutcome::Duplicate);
                }
                let retained_delegation = state.kind == WorkflowKind::Delegation
                    && matches!(actor.lifecycle, WorkflowActorLifecycle::Completed);
                if !matches!(actor.lifecycle, WorkflowActorLifecycle::Waiting { .. })
                    && !retained_delegation
                {
                    return Err(Self::error(
                        event.workflow_id,
                        "only a waiting or retained delegation actor can resume",
                    ));
                }
                actor.lifecycle = WorkflowActorLifecycle::Running;
                actor.resume_count = actor.resume_count.saturating_add(1);
                state.waiting = None;
            }
            WorkflowTransition::ActorFinished { actor_id, outcome } => {
                let Some(actor) = state.actors.get_mut(actor_id) else {
                    return Err(Self::error(
                        event.workflow_id,
                        "cannot finish an unknown actor",
                    ));
                };
                let lifecycle = match outcome {
                    SubagentOutcome::Completed => WorkflowActorLifecycle::Completed,
                    SubagentOutcome::Cancelled => WorkflowActorLifecycle::Cancelled,
                    SubagentOutcome::Failed(message) => {
                        WorkflowActorLifecycle::Failed(message.clone())
                    }
                };
                if actor.lifecycle == lifecycle {
                    return Ok(ApplyOutcome::Duplicate);
                }
                if actor.lifecycle.is_terminal() {
                    return Err(Self::error(
                        event.workflow_id,
                        "actor terminal outcome cannot change",
                    ));
                }
                actor.lifecycle = lifecycle;
            }
            WorkflowTransition::Waiting {
                dependency,
                remaining,
                requires_user_action,
            } => {
                if dependency.trim().is_empty() {
                    return Err(Self::error(
                        event.workflow_id,
                        "waiting dependency is empty",
                    ));
                }
                let waiting = WorkflowWait {
                    dependency: dependency.clone(),
                    remaining: *remaining,
                    requires_user_action: *requires_user_action,
                };
                if state.waiting.as_ref() == Some(&waiting) {
                    return Ok(ApplyOutcome::Duplicate);
                }
                state.waiting = Some(waiting);
            }
            WorkflowTransition::CoverageChanged { coverage } => {
                if state.coverage == *coverage {
                    return Ok(ApplyOutcome::Duplicate);
                }
                if state.coverage == WorkflowCoverage::Degraded
                    && *coverage == WorkflowCoverage::Complete
                {
                    return Err(Self::error(
                        event.workflow_id,
                        "degraded coverage cannot become complete in the same workflow",
                    ));
                }
                state.coverage = *coverage;
            }
            WorkflowTransition::IssuesValidated { pass, summaries } => {
                if summaries.is_empty() {
                    return Err(Self::error(
                        event.workflow_id,
                        "validated issue list is empty",
                    ));
                }
                if state.issues.iter().any(|issue| issue.pass == *pass) {
                    return Ok(ApplyOutcome::Duplicate);
                }
                let first_id = state.issues.len() + 1;
                state
                    .issues
                    .extend(
                        summaries
                            .iter()
                            .enumerate()
                            .map(|(index, summary)| ReviewIssue {
                                id: first_id + index,
                                pass: *pass,
                                summary: summary.clone(),
                                status: ReviewIssueStatus::Validated,
                                resolution_reason: None,
                                resolution_details: None,
                            }),
                    );
            }
            WorkflowTransition::IssuesResolved {
                pass,
                summaries,
                status,
                reason,
                details,
            } => {
                let mut changed = false;
                for issue in state.issues.iter_mut().filter(|issue| {
                    issue.pass == *pass
                        && summaries
                            .as_ref()
                            .is_none_or(|summaries| summaries.contains(&issue.summary))
                }) {
                    if issue.status != *status {
                        issue.status = *status;
                        issue.resolution_reason = reason.clone();
                        if details.is_some() {
                            issue.resolution_details = details.clone();
                        }
                        changed = true;
                    }
                }
                if !changed {
                    return Ok(ApplyOutcome::Duplicate);
                }
            }
            WorkflowTransition::IssueEvidenceUpdated {
                pass,
                summaries,
                reason,
                details,
            } => {
                let mut changed = false;
                for issue in state.issues.iter_mut().filter(|issue| {
                    issue.pass == *pass
                        && summaries
                            .as_ref()
                            .is_none_or(|summaries| summaries.contains(&issue.summary))
                }) {
                    if issue.status != ReviewIssueStatus::Corrected {
                        return Err(Self::error(
                            event.workflow_id,
                            "correction evidence can update only corrected issues",
                        ));
                    }
                    if issue.resolution_reason.as_deref() != Some(reason)
                        || issue.resolution_details.as_deref() != Some(details)
                    {
                        issue.resolution_reason = Some(reason.clone());
                        issue.resolution_details = Some(details.clone());
                        changed = true;
                    }
                }
                if !changed {
                    return Ok(ApplyOutcome::Duplicate);
                }
            }
            WorkflowTransition::Terminal { outcome, coverage } => {
                if state.coverage == WorkflowCoverage::Degraded
                    && *coverage == WorkflowCoverage::Complete
                {
                    return Err(Self::error(
                        event.workflow_id,
                        "terminal coverage cannot undo degraded coverage",
                    ));
                }
                if matches!(outcome, WorkflowOutcome::Clean)
                    && (*coverage != WorkflowCoverage::Complete
                        || state.actors.values().any(|actor| {
                            matches!(
                                actor.lifecycle,
                                WorkflowActorLifecycle::Failed(_)
                                    | WorkflowActorLifecycle::Cancelled
                            )
                        }))
                {
                    return Err(Self::error(
                        event.workflow_id,
                        "clean completion requires complete coverage and no failed actors",
                    ));
                }
                if state.unfinished_count() > 0 {
                    return Err(Self::error(
                        event.workflow_id,
                        "workflow cannot terminate while actors are still running or waiting",
                    ));
                }
                state.stage = WorkflowStage::new(state.stage.pass, WorkflowPhase::Terminal);
                state.waiting = None;
                state.coverage = *coverage;
                state.outcome = Some(*outcome);
            }
        }
        Ok(ApplyOutcome::Changed)
    }

    fn make_room(&mut self) {
        while self.states.len() >= MAX_RETAINED_WORKFLOWS {
            let oldest_terminal = self
                .states
                .iter()
                .find_map(|(id, state)| state.outcome.is_some().then_some(*id));
            let oldest = oldest_terminal.or_else(|| self.states.keys().next().copied());
            let Some(oldest) = oldest else {
                break;
            };
            self.states.remove(&oldest);
        }
    }

    fn error(workflow_id: WorkflowId, message: &str) -> WorkflowTransitionError {
        WorkflowTransitionError {
            workflow_id,
            message: message.to_string(),
        }
    }
}

/// Applies transitions once at their runtime source and publishes only valid
/// state changes. Consumers replay the same transition through [`WorkflowStore`].
#[derive(Clone)]
pub struct WorkflowEmitter {
    inner: Arc<WorkflowEmitterInner>,
}

struct WorkflowEmitterInner {
    store: Mutex<WorkflowStore>,
    events: mpsc::UnboundedSender<UiEvent>,
}

impl fmt::Debug for WorkflowEmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkflowEmitter").finish_non_exhaustive()
    }
}

impl WorkflowEmitter {
    pub fn new(events: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            inner: Arc::new(WorkflowEmitterInner {
                store: Mutex::new(WorkflowStore::default()),
                events,
            }),
        }
    }

    pub fn emit(&self, event: WorkflowEvent) -> Result<ApplyOutcome, WorkflowTransitionError> {
        let mut store = self
            .inner
            .store
            .lock()
            .expect("workflow state lock poisoned");
        let outcome = store.apply(&event)?;
        if outcome == ApplyOutcome::Changed {
            let _ = self.inner.events.send(UiEvent::Workflow(event));
        }
        Ok(outcome)
    }

    pub fn state(&self, id: WorkflowId) -> Option<WorkflowState> {
        self.inner
            .store
            .lock()
            .expect("workflow state lock poisoned")
            .get(id)
            .cloned()
    }
}

/// Workflow metadata carried by a Mjolnir-owned programmatic subagent. The
/// worker binds the actual ACP session id after `session/new`; orchestration
/// code still owns phase, waiting-count, and verdict transitions.
#[derive(Debug, Clone)]
pub struct WorkflowActorContext {
    pub emitter: WorkflowEmitter,
    pub workflow_id: WorkflowId,
    pub role: WorkflowActorRole,
}

impl WorkflowActorContext {
    pub fn actor_id(subagent_id: u64) -> WorkflowActorId {
        WorkflowActorId::Subagent(subagent_id)
    }

    pub fn started(&self, subagent_id: u64) {
        let _ = self.emitter.emit(WorkflowEvent::new(
            self.workflow_id,
            WorkflowTransition::ActorStarted {
                actor_id: Self::actor_id(subagent_id),
                role: self.role.clone(),
            },
        ));
    }

    pub fn session_bound(&self, subagent_id: u64, session_id: String) {
        let _ = self.emitter.emit(WorkflowEvent::new(
            self.workflow_id,
            WorkflowTransition::ActorSessionBound {
                actor_id: Self::actor_id(subagent_id),
                retained_session_id: session_id,
            },
        ));
    }

    pub fn resumed(&self, subagent_id: u64) {
        let _ = self.emitter.emit(WorkflowEvent::new(
            self.workflow_id,
            WorkflowTransition::ActorResumed {
                actor_id: Self::actor_id(subagent_id),
            },
        ));
    }

    pub fn finished(&self, subagent_id: u64, outcome: SubagentOutcome) {
        let _ = self.emitter.emit(WorkflowEvent::new(
            self.workflow_id,
            WorkflowTransition::ActorFinished {
                actor_id: Self::actor_id(subagent_id),
                outcome,
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> WorkflowId {
        WorkflowId::review(7)
    }

    fn started() -> WorkflowEvent {
        WorkflowEvent::new(
            review(),
            WorkflowTransition::Started {
                kind: WorkflowKind::Review,
                stage: WorkflowStage::new(0, WorkflowPhase::IntentAnalysis),
            },
        )
    }

    #[test]
    fn supervisor_wait_and_resume_stay_one_actor_and_workflow() {
        let mut store = WorkflowStore::default();
        assert_eq!(store.apply(&started()).unwrap(), ApplyOutcome::Changed);
        let supervisor = WorkflowActorId::Subagent(2);
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::ActorStarted {
                    actor_id: supervisor.clone(),
                    role: WorkflowActorRole::ReviewSupervisor,
                },
            ))
            .unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::ActorWaiting {
                    actor_id: supervisor.clone(),
                    dependency: "specialist reviewer reports".to_string(),
                    remaining: Some(3),
                    requires_user_action: false,
                },
            ))
            .unwrap();
        assert_eq!(store.get(review()).unwrap().running_count(), 0);
        assert_eq!(store.get(review()).unwrap().waiting_count(), 1);
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::ActorResumed {
                    actor_id: supervisor.clone(),
                },
            ))
            .unwrap();

        let state = store.get(review()).unwrap();
        assert_eq!(state.actors.len(), 1);
        assert_eq!(state.actors[&supervisor].resume_count, 1);
        assert_eq!(state.running_count(), 1);
        assert_eq!(state.waiting_count(), 0);
        assert!(matches!(
            state.actors[&supervisor].lifecycle,
            WorkflowActorLifecycle::Running
        ));
    }

    #[test]
    fn phase_and_terminal_state_cannot_regress() {
        let mut store = WorkflowStore::default();
        store.apply(&started()).unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::PhaseChanged {
                    stage: WorkflowStage::new(0, WorkflowPhase::Synthesis),
                },
            ))
            .unwrap();
        let error = store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::PhaseChanged {
                    stage: WorkflowStage::new(0, WorkflowPhase::Supervision),
                },
            ))
            .unwrap_err();
        assert!(error.message.contains("regress"));

        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::Terminal {
                    outcome: WorkflowOutcome::Clean,
                    coverage: WorkflowCoverage::Complete,
                },
            ))
            .unwrap();
        assert!(
            store
                .apply(&WorkflowEvent::new(
                    review(),
                    WorkflowTransition::Waiting {
                        dependency: "late report".to_string(),
                        remaining: Some(1),
                        requires_user_action: false,
                    },
                ))
                .is_err()
        );
    }

    #[test]
    fn clean_cannot_hide_failed_coverage() {
        let mut store = WorkflowStore::default();
        store.apply(&started()).unwrap();
        let reviewer = WorkflowActorId::Subagent(4);
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::ActorStarted {
                    actor_id: reviewer.clone(),
                    role: WorkflowActorRole::SpecialistReviewer {
                        lane: "heimdall".to_string(),
                    },
                },
            ))
            .unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::ActorFinished {
                    actor_id: reviewer,
                    outcome: SubagentOutcome::Failed("adapter exited".to_string()),
                },
            ))
            .unwrap();
        let error = store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::Terminal {
                    outcome: WorkflowOutcome::Clean,
                    coverage: WorkflowCoverage::Complete,
                },
            ))
            .unwrap_err();
        assert!(error.message.contains("clean completion"));
    }

    #[test]
    fn duplicate_and_stale_events_are_idempotent_or_rejected() {
        let mut store = WorkflowStore::default();
        assert_eq!(store.apply(&started()).unwrap(), ApplyOutcome::Changed);
        assert_eq!(store.apply(&started()).unwrap(), ApplyOutcome::Duplicate);
        assert!(
            store
                .apply(&WorkflowEvent::new(
                    WorkflowId::review(6),
                    WorkflowTransition::Waiting {
                        dependency: "reviewers".to_string(),
                        remaining: Some(1),
                        requires_user_action: false,
                    },
                ))
                .is_err()
        );
    }

    #[test]
    fn review_issues_distinguish_unverified_corrections_from_verified_fixes() {
        let mut store = WorkflowStore::default();
        store.apply(&started()).unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesValidated {
                    pass: 0,
                    summaries: vec!["[P1] src/a.rs:1 -- broken".to_string()],
                },
            ))
            .unwrap();
        assert_eq!(
            store.get(review()).unwrap().issues[0].status,
            ReviewIssueStatus::Validated
        );

        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesResolved {
                    pass: 0,
                    summaries: None,
                    status: ReviewIssueStatus::Corrected,
                    reason: Some(
                        "correction changed the workspace; verification is pending".to_string(),
                    ),
                    details: Some("exact correction diff".to_string()),
                },
            ))
            .unwrap();
        assert_eq!(
            store.get(review()).unwrap().issues[0].status,
            ReviewIssueStatus::Corrected
        );
        assert_eq!(
            store.get(review()).unwrap().issues[0].resolution_reason,
            Some("correction changed the workspace; verification is pending".to_string())
        );
        assert_eq!(
            store.get(review()).unwrap().issues[0].resolution_details,
            Some("exact correction diff".to_string())
        );

        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssueEvidenceUpdated {
                    pass: 0,
                    summaries: None,
                    reason: "the correction validation completed; verification is pending"
                        .to_string(),
                    details: "final correction report and exact diff".to_string(),
                },
            ))
            .unwrap();
        assert_eq!(
            store.get(review()).unwrap().issues[0].status,
            ReviewIssueStatus::Corrected
        );
        assert_eq!(
            store.get(review()).unwrap().issues[0].resolution_reason,
            Some("the correction validation completed; verification is pending".to_string())
        );
        assert_eq!(
            store.get(review()).unwrap().issues[0].resolution_details,
            Some("final correction report and exact diff".to_string())
        );

        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesResolved {
                    pass: 0,
                    summaries: None,
                    status: ReviewIssueStatus::Fixed,
                    reason: Some(
                        "verification review pass 2 returned clean after the correction"
                            .to_string(),
                    ),
                    details: None,
                },
            ))
            .unwrap();
        assert_eq!(
            store.get(review()).unwrap().issues[0].status,
            ReviewIssueStatus::Fixed
        );
        assert_eq!(
            store.get(review()).unwrap().issues[0].resolution_details,
            Some("final correction report and exact diff".to_string()),
            "verification preserves the exact correction evidence"
        );

        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesValidated {
                    pass: 1,
                    summaries: vec!["[P2] src/b.rs:2 -- stale claim".to_string()],
                },
            ))
            .unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesResolved {
                    pass: 1,
                    summaries: None,
                    status: ReviewIssueStatus::Uncorrected,
                    reason: Some("correction turn changed nothing in the workspace; this finding remains unresolved".to_string()),
                    details: Some("no correction diff".to_string()),
                },
            ))
            .unwrap();
        assert_eq!(
            store.get(review()).unwrap().issues[1].status,
            ReviewIssueStatus::Uncorrected
        );
        assert_eq!(
            store.get(review()).unwrap().issues[1].resolution_reason,
            Some(
                "correction turn changed nothing in the workspace; this finding remains unresolved"
                    .to_string()
            )
        );

        let state = store.get(review()).unwrap();
        assert_eq!(
            state.issue_tally(),
            ReviewIssueTally {
                found: 2,
                open: 0,
                corrected: 0,
                fixed: 1,
                deferred: 0,
                uncorrected: 1,
                invalidated: 0,
            }
        );
        assert_eq!(
            state.terminal_notice(WorkflowOutcome::Completed),
            "review complete · 2 issues · 1 verified fixed · 1 unresolved"
        );
    }

    #[test]
    fn targeted_resolution_defers_only_lower_priority_validated_findings() {
        let mut store = WorkflowStore::default();
        store.apply(&started()).expect("start review");
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesValidated {
                    pass: 0,
                    summaries: vec![
                        "[P1] src/retry.rs:12 -- retries drop the final error".to_string(),
                        "[P2] src/header.rs:1 -- license header could be normalized".to_string(),
                    ],
                },
            ))
            .expect("validate findings");
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::IssuesResolved {
                    pass: 0,
                    summaries: Some(vec![
                        "[P2] src/header.rs:1 -- license header could be normalized".to_string(),
                    ]),
                    status: ReviewIssueStatus::Deferred,
                    reason: Some(
                        "validated finding is below the automatic correction threshold P1; it remains tracked but was not sent to the primary".to_string(),
                    ),
                    details: None,
                },
            ))
            .expect("defer P2");

        let state = store.get(review()).expect("review state");
        assert_eq!(state.issues[0].status, ReviewIssueStatus::Validated);
        assert_eq!(state.issues[1].status, ReviewIssueStatus::Deferred);
        assert!(
            state.issues[1]
                .resolution_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("threshold P1"))
        );
        assert_eq!(state.issue_tally().open, 1);
        assert_eq!(state.issue_tally().deferred, 1);
    }

    #[test]
    fn terminal_transition_cannot_restore_degraded_coverage() {
        let mut store = WorkflowStore::default();
        store.apply(&started()).unwrap();
        store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::CoverageChanged {
                    coverage: WorkflowCoverage::Degraded,
                },
            ))
            .unwrap();

        let error = store
            .apply(&WorkflowEvent::new(
                review(),
                WorkflowTransition::Terminal {
                    outcome: WorkflowOutcome::Degraded,
                    coverage: WorkflowCoverage::Complete,
                },
            ))
            .unwrap_err();

        assert!(error.message.contains("cannot undo degraded coverage"));
        assert_eq!(
            store.get(review()).unwrap().coverage,
            WorkflowCoverage::Degraded
        );
        assert_eq!(store.get(review()).unwrap().outcome, None);
    }

    #[test]
    fn store_retains_only_the_newest_bounded_workflow_history() {
        let mut store = WorkflowStore::default();
        let total = MAX_RETAINED_WORKFLOWS as u64 + 5;
        for turn_id in 1..=total {
            let workflow_id = WorkflowId::delegation(turn_id);
            store
                .apply(&WorkflowEvent::new(
                    workflow_id,
                    WorkflowTransition::Started {
                        kind: WorkflowKind::Delegation,
                        stage: WorkflowStage::new(0, WorkflowPhase::Delegating),
                    },
                ))
                .unwrap();
            store
                .apply(&WorkflowEvent::new(
                    workflow_id,
                    WorkflowTransition::Terminal {
                        outcome: WorkflowOutcome::Completed,
                        coverage: WorkflowCoverage::Complete,
                    },
                ))
                .unwrap();
        }

        assert_eq!(store.len(), MAX_RETAINED_WORKFLOWS);
        assert!(store.get(WorkflowId::delegation(1)).is_none());
        assert!(store.get(WorkflowId::delegation(total)).is_some());
    }
}
