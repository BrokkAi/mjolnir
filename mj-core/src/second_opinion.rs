//! Cross-profile plan review: choosing the reviewer and remembering the choice.
//!
//! A second opinion runs a reviewer harness beside the primary session, in the
//! primary's own target and working directory. Which harness that is comes
//! from a waterfall: pick a configured profile, discover what that profile's
//! harness actually advertises, then pick a model and an effort from the
//! advertised values.
//!
//! Discovery is asynchronous and lives outside this module: [`ReviewerSetup`]
//! is a pure state machine that says what to probe next and what the view
//! should show, and the caller reports each result back. Every probe carries a
//! generation, so a result that arrives after the user changed an earlier
//! choice is discarded instead of overwriting the newer selection.
//!
//! [`ReviewWorkflow`] is the review itself, on the same terms: it owns the
//! captured proposal, the order the two agents are prompted in, and the wording
//! of every generated prompt, while the caller owns the transport. Each step
//! names the command it awaits, so replaying an old completion after a
//! reconnect cannot send feedback twice or start a second reviewer turn.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::acp::{SessionConfigChoice, session_config_choices};
use agent_client_protocol::schema::v1::SessionConfigOption;

/// Label shown when a harness advertises no model or no effort selector. It is
/// a choice the user confirms, not a value Hel sends to the harness.
pub const HARNESS_DEFAULT_LABEL: &str = "Harness default";

/// One configured profile the reviewer can run under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerProfileChoice {
    pub id: String,
    /// Harness kind label, shown beside the id so two profiles of the same
    /// harness stay distinguishable.
    pub harness: String,
}

/// Which step of the waterfall has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStage {
    Profile,
    Model,
    Effort,
}

/// Work the caller must perform on the state machine's behalf.
///
/// Each request carries the generation it belongs to. The caller passes that
/// generation back with the result, and a result whose generation is no longer
/// current is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupRequest {
    /// Start a provisional reviewer harness under `profile_id` and report the
    /// session configuration options it advertises.
    Probe { generation: u64, profile_id: String },
    /// Apply `model` on the provisional reviewer and report the refreshed
    /// options, since a model change can change the available efforts.
    ApplyModel { generation: u64, model: String },
    /// Stop the provisional reviewer started for `generation`. The caller
    /// receives this for an obsolete probe and when setup is abandoned.
    CancelProbe { generation: u64 },
}

/// What confirming the last step produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerSelection {
    pub profile_id: String,
    /// `None` when the harness advertises no model selector.
    pub model: Option<String>,
    /// `None` when the harness advertises no effort selector.
    pub effort: Option<String>,
}

/// What one key press asked the setup dialog to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    /// Nothing to do; the view changed only its highlight.
    None,
    /// Work to perform, in order: obsolete reviewers are stopped before a new
    /// one starts, so an abandoned probe never outlives its replacement.
    Requests(Vec<SetupRequest>),
    /// The waterfall is complete. The reviewer discovery started keeps
    /// running and becomes the review's reviewer, so nothing is stopped.
    Confirmed { selection: ReviewerSelection },
    /// The user abandoned setup, leaving these reviewers to stop.
    Cancelled { requests: Vec<SetupRequest> },
}

/// Discovery state for the currently highlighted profile.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Discovery {
    /// No profile confirmed yet.
    Idle,
    /// A reviewer harness is starting so its options can be read.
    Probing,
    /// Options are known and the user is choosing among them.
    Ready {
        models: Vec<SessionConfigChoice>,
        efforts: Vec<SessionConfigChoice>,
    },
    /// A model is being applied and refreshed efforts are awaited.
    Configuring {
        models: Vec<SessionConfigChoice>,
        efforts: Vec<SessionConfigChoice>,
    },
    /// Discovery or configuration failed. Retry and cancel remain available.
    Failed { message: String },
}

/// The profile/model/effort waterfall.
#[derive(Debug, Clone)]
pub struct ReviewerSetup {
    workspace_id: String,
    profiles: Vec<ReviewerProfileChoice>,
    profile_index: usize,
    stage: SetupStage,
    discovery: Discovery,
    model_index: usize,
    effort_index: usize,
    /// Bumped whenever an earlier choice changes, which retires every probe
    /// started under the previous value.
    generation: u64,
    /// The provisional reviewer that may still be running, if any. At most one
    /// exists at a time: every step that starts a probe retires the last.
    live_probe: Option<u64>,
    defaults: ReviewerDefaults,
}

impl ReviewerSetup {
    /// Opens the waterfall on `profiles`, preselecting whatever this workspace
    /// confirmed last. A remembered profile that is no longer configured is
    /// ignored rather than carried forward.
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        profiles: Vec<ReviewerProfileChoice>,
        defaults: ReviewerDefaults,
    ) -> Self {
        let workspace_id = workspace_id.into();
        let profile_index = defaults
            .profile(&workspace_id)
            .and_then(|remembered| profiles.iter().position(|profile| profile.id == remembered))
            .unwrap_or_default();
        Self {
            workspace_id,
            profiles,
            profile_index,
            stage: SetupStage::Profile,
            discovery: Discovery::Idle,
            model_index: 0,
            effort_index: 0,
            generation: 0,
            live_probe: None,
            defaults,
        }
    }

    #[must_use]
    pub fn stage(&self) -> SetupStage {
        self.stage
    }

    #[must_use]
    pub fn profiles(&self) -> &[ReviewerProfileChoice] {
        &self.profiles
    }

    #[must_use]
    pub fn profile_index(&self) -> usize {
        self.profile_index
    }

    #[must_use]
    pub fn model_index(&self) -> usize {
        self.model_index
    }

    #[must_use]
    pub fn effort_index(&self) -> usize {
        self.effort_index
    }

    /// The model rows to draw, or an empty slice while discovery is running.
    #[must_use]
    pub fn models(&self) -> &[SessionConfigChoice] {
        match &self.discovery {
            Discovery::Ready { models, .. } | Discovery::Configuring { models, .. } => models,
            _ => &[],
        }
    }

    /// The effort rows to draw, or an empty slice while a model is being
    /// applied and the refreshed options are still outstanding.
    #[must_use]
    pub fn efforts(&self) -> &[SessionConfigChoice] {
        match &self.discovery {
            Discovery::Ready { efforts, .. } => efforts,
            _ => &[],
        }
    }

    /// Whether a probe or a configuration change is still outstanding. The
    /// view disables confirmation while this holds.
    #[must_use]
    pub fn busy(&self) -> bool {
        matches!(
            self.discovery,
            Discovery::Probing | Discovery::Configuring { .. }
        )
    }

    /// The failure to show, with retry and cancel offered beside it.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        match &self.discovery {
            Discovery::Failed { message } => Some(message.as_str()),
            _ => None,
        }
    }

    /// Whether confirming the current step is allowed right now.
    #[must_use]
    pub fn can_confirm(&self) -> bool {
        if self.busy() || self.failure().is_some() {
            return false;
        }
        match self.stage {
            SetupStage::Profile => !self.profiles.is_empty(),
            SetupStage::Model => !self.models().is_empty(),
            SetupStage::Effort => !self.efforts().is_empty(),
        }
    }

    /// Moves the highlight within the current step.
    pub fn move_selection(&mut self, delta: isize) {
        let len = match self.stage {
            SetupStage::Profile => self.profiles.len(),
            SetupStage::Model => self.models().len(),
            SetupStage::Effort => self.efforts().len(),
        };
        if len == 0 {
            return;
        }
        let index = match self.stage {
            SetupStage::Profile => &mut self.profile_index,
            SetupStage::Model => &mut self.model_index,
            SetupStage::Effort => &mut self.effort_index,
        };
        *index = if delta.is_negative() {
            index.checked_sub(1).unwrap_or(len - 1)
        } else {
            (*index + 1) % len
        };
    }

    /// Confirms the highlighted row and advances the waterfall.
    pub fn confirm(&mut self) -> SetupOutcome {
        if !self.can_confirm() {
            return SetupOutcome::None;
        }
        match self.stage {
            SetupStage::Profile => {
                let Some(profile) = self.profiles.get(self.profile_index).cloned() else {
                    return SetupOutcome::None;
                };
                let mut requests = self.retire_live_probe();
                let generation = self.next_generation();
                self.discovery = Discovery::Probing;
                self.stage = SetupStage::Model;
                requests.push(SetupRequest::Probe {
                    generation,
                    profile_id: profile.id,
                });
                SetupOutcome::Requests(requests)
            }
            SetupStage::Model => {
                let Some(model) = self.models().get(self.model_index).cloned() else {
                    return SetupOutcome::None;
                };
                self.stage = SetupStage::Effort;
                // A harness with no model selector has nothing to apply, and
                // its effort list is already the one discovery reported.
                if model.value == HARNESS_DEFAULT_VALUE {
                    self.effort_index = self.remembered_effort_index(&model.value);
                    return SetupOutcome::None;
                }
                let Discovery::Ready { models, efforts } = self.discovery.clone() else {
                    return SetupOutcome::None;
                };
                self.discovery = Discovery::Configuring { models, efforts };
                SetupOutcome::Requests(vec![SetupRequest::ApplyModel {
                    generation: self.generation,
                    model: model.value,
                }])
            }
            SetupStage::Effort => {
                let Some(effort) = self.efforts().get(self.effort_index).cloned() else {
                    return SetupOutcome::None;
                };
                let Some(profile) = self.profiles.get(self.profile_index).cloned() else {
                    return SetupOutcome::None;
                };
                let model = self
                    .models()
                    .get(self.model_index)
                    .map(|choice| choice.value.clone());
                let selection = ReviewerSelection {
                    profile_id: profile.id,
                    model: model.filter(|value| value != HARNESS_DEFAULT_VALUE),
                    effort: Some(effort.value).filter(|value| value != HARNESS_DEFAULT_VALUE),
                };
                self.defaults.remember(&self.workspace_id, &selection);
                // The reviewer discovery started is the reviewer the review
                // will use, so it is handed over rather than stopped.
                self.live_probe = None;
                SetupOutcome::Confirmed { selection }
            }
        }
    }

    /// Steps back to the previous choice. Changing an earlier choice retires
    /// every probe started under the old one, so a result still in flight can
    /// no longer overwrite the newer selection.
    ///
    /// Stepping back is refused while a probe or a model change is running:
    /// the reviewer's live configuration would otherwise disagree with what
    /// the dialog shows.
    pub fn back(&mut self) -> SetupOutcome {
        if self.busy() {
            return SetupOutcome::None;
        }
        match self.stage {
            SetupStage::Profile => SetupOutcome::None,
            SetupStage::Model => {
                self.stage = SetupStage::Profile;
                self.discovery = Discovery::Idle;
                SetupOutcome::Requests(self.retire_live_probe())
            }
            SetupStage::Effort => {
                self.stage = SetupStage::Model;
                SetupOutcome::None
            }
        }
    }

    /// Retries the failed step with a fresh generation.
    pub fn retry(&mut self) -> SetupOutcome {
        if self.failure().is_none() {
            return SetupOutcome::None;
        }
        let Some(profile) = self.profiles.get(self.profile_index).cloned() else {
            return SetupOutcome::None;
        };
        let mut requests = self.retire_live_probe();
        let generation = self.next_generation();
        self.discovery = Discovery::Probing;
        self.stage = SetupStage::Model;
        requests.push(SetupRequest::Probe {
            generation,
            profile_id: profile.id,
        });
        SetupOutcome::Requests(requests)
    }

    /// Abandons setup, naming every provisional reviewer the caller must stop.
    pub fn cancel(&mut self) -> SetupOutcome {
        SetupOutcome::Cancelled {
            requests: self.retire_live_probe(),
        }
    }

    /// Reports what a profile's harness advertises. A result from a retired
    /// generation is ignored, and its reviewer is named for teardown.
    pub fn probe_succeeded(
        &mut self,
        generation: u64,
        options: &[SessionConfigOption],
    ) -> Option<SetupRequest> {
        if let Some(stale) = self.reject_stale(generation) {
            return Some(stale);
        }
        let models = with_harness_default(session_config_choices(options, "model"));
        let efforts = with_harness_default(session_config_choices(options, "effort"));
        self.model_index = self.remembered_model_index(&models);
        self.discovery = Discovery::Ready { models, efforts };
        self.stage = SetupStage::Model;
        None
    }

    /// Reports the options a model change produced, refreshing the efforts.
    pub fn model_applied(
        &mut self,
        generation: u64,
        options: &[SessionConfigOption],
    ) -> Option<SetupRequest> {
        if let Some(stale) = self.reject_stale(generation) {
            return Some(stale);
        }
        let Discovery::Configuring { models, .. } = self.discovery.clone() else {
            return None;
        };
        let efforts = with_harness_default(session_config_choices(options, "effort"));
        let model = models
            .get(self.model_index)
            .map(|choice| choice.value.clone())
            .unwrap_or_default();
        self.discovery = Discovery::Ready { models, efforts };
        self.effort_index = self.remembered_effort_index(&model);
        self.stage = SetupStage::Effort;
        None
    }

    /// Reports that the step now running failed, whichever generation it
    /// belongs to. Callers that lost track of the generation — a staging
    /// failure before any probe started, say — use this.
    pub fn probe_failed_current(&mut self, message: impl Into<String>) {
        let generation = self.generation;
        self.probe_failed(generation, message);
    }

    /// Reports that discovery or configuration failed.
    pub fn probe_failed(&mut self, generation: u64, message: impl Into<String>) {
        if self.reject_stale(generation).is_some() {
            return;
        }
        self.discovery = Discovery::Failed {
            message: message.into(),
        };
    }

    /// The defaults this setup has learned, for the caller to persist.
    #[must_use]
    pub fn defaults(&self) -> &ReviewerDefaults {
        &self.defaults
    }

    /// Whether `generation` is obsolete, and the teardown it then needs.
    ///
    /// A retired generation's reviewer was already stopped when it was
    /// retired; naming it again is harmless and covers the race where the
    /// harness answered before the stop reached it.
    fn reject_stale(&mut self, generation: u64) -> Option<SetupRequest> {
        (generation != self.generation).then_some(SetupRequest::CancelProbe { generation })
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.live_probe = Some(self.generation);
        self.generation
    }

    fn retire_live_probe(&mut self) -> Vec<SetupRequest> {
        self.live_probe
            .take()
            .map(|generation| SetupRequest::CancelProbe { generation })
            .into_iter()
            .collect()
    }

    fn current_profile_id(&self) -> Option<&str> {
        self.profiles
            .get(self.profile_index)
            .map(|profile| profile.id.as_str())
    }

    /// The remembered model's row, or the first row when the remembered value
    /// is no longer advertised.
    fn remembered_model_index(&self, models: &[SessionConfigChoice]) -> usize {
        let Some(profile_id) = self.current_profile_id() else {
            return 0;
        };
        self.defaults
            .model(&self.workspace_id, profile_id)
            .and_then(|remembered| models.iter().position(|choice| choice.value == remembered))
            .unwrap_or_default()
    }

    fn remembered_effort_index(&self, model: &str) -> usize {
        let Some(profile_id) = self.current_profile_id() else {
            return 0;
        };
        self.defaults
            .effort(&self.workspace_id, profile_id, model)
            .and_then(|remembered| {
                self.efforts()
                    .iter()
                    .position(|choice| choice.value == remembered)
            })
            .unwrap_or_default()
    }
}

/// Sentinel value for the single choice offered when a harness advertises no
/// selector. It is never sent to a harness.
pub const HARNESS_DEFAULT_VALUE: &str = "\u{0}harness-default";

/// The advertised choices, or the single harness-default row when the harness
/// advertises none.
fn with_harness_default(choices: Vec<SessionConfigChoice>) -> Vec<SessionConfigChoice> {
    if !choices.is_empty() {
        return choices;
    }
    vec![SessionConfigChoice {
        value: HARNESS_DEFAULT_VALUE.to_owned(),
        name: HARNESS_DEFAULT_LABEL.to_owned(),
        description: Some("This harness does not expose a choice here".to_owned()),
    }]
}

/// What each workspace last confirmed, so a repeat review does not ask again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerDefaults {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workspaces: BTreeMap<String, WorkspaceReviewerDefaults>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceReviewerDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    /// Model confirmed for each profile.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    models: BTreeMap<String, String>,
    /// Effort confirmed for each profile and model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    efforts: BTreeMap<String, BTreeMap<String, String>>,
}

impl ReviewerSelection {
    /// The three values a store keeps, with a harness default written as its
    /// sentinel so the same row is preselected next time.
    #[must_use]
    pub fn stored_values(&self) -> (&str, &str, &str) {
        (
            &self.profile_id,
            self.model.as_deref().unwrap_or(HARNESS_DEFAULT_VALUE),
            self.effort.as_deref().unwrap_or(HARNESS_DEFAULT_VALUE),
        )
    }
}

impl ReviewerDefaults {
    /// Rebuilds one remembered row, as a store reads it back.
    pub fn restore(&mut self, workspace_id: &str, profile_id: &str, model: &str, effort: &str) {
        let workspace = self.workspaces.entry(workspace_id.to_owned()).or_default();
        workspace.profile = Some(profile_id.to_owned());
        workspace
            .models
            .insert(profile_id.to_owned(), model.to_owned());
        workspace
            .efforts
            .entry(profile_id.to_owned())
            .or_default()
            .insert(model.to_owned(), effort.to_owned());
    }

    #[must_use]
    pub fn profile(&self, workspace_id: &str) -> Option<&str> {
        self.workspaces.get(workspace_id)?.profile.as_deref()
    }

    #[must_use]
    pub fn model(&self, workspace_id: &str, profile_id: &str) -> Option<&str> {
        self.workspaces
            .get(workspace_id)?
            .models
            .get(profile_id)
            .map(String::as_str)
    }

    #[must_use]
    pub fn effort(&self, workspace_id: &str, profile_id: &str, model: &str) -> Option<&str> {
        self.workspaces
            .get(workspace_id)?
            .efforts
            .get(profile_id)?
            .get(model)
            .map(String::as_str)
    }

    /// Records a confirmed selection. A harness-default choice is stored under
    /// its sentinel so the same row is preselected next time.
    pub fn remember(&mut self, workspace_id: &str, selection: &ReviewerSelection) {
        let workspace = self.workspaces.entry(workspace_id.to_owned()).or_default();
        workspace.profile = Some(selection.profile_id.clone());
        let model = selection
            .model
            .clone()
            .unwrap_or_else(|| HARNESS_DEFAULT_VALUE.to_owned());
        workspace
            .models
            .insert(selection.profile_id.clone(), model.clone());
        let effort = selection
            .effort
            .clone()
            .unwrap_or_else(|| HARNESS_DEFAULT_VALUE.to_owned());
        workspace
            .efforts
            .entry(selection.profile_id.clone())
            .or_default()
            .insert(model, effort);
    }
}

/// Opening marker of every prompt Hel generates for a review.
///
/// These are control-origin records: Hel wrote them, not the user, and the
/// transcript says so rather than attributing them to whoever asked for the
/// review. Generating and recognizing them share this one marker.
pub const HARNESS_NOTE_MARKER: &str = "[HARNESS NOTE:";

/// Whether this prompt text is one Hel generated rather than one a person
/// typed.
#[must_use]
pub fn is_control_origin_prompt(text: &str) -> bool {
    text.trim_start().starts_with(HARNESS_NOTE_MARKER)
}

/// The harness note that asks the primary for the context a reviewer needs.
///
/// The plan itself is already captured, so the primary is asked only for what
/// Hel cannot reconstruct: what the user wanted and why the plan looks as it
/// does.
pub const PRIMARY_CONTEXT_REQUEST: &str = "[HARNESS NOTE: the user wants to get a second opinion before approving this plan. I have captured your plan proposal; you do not need to repeat that. What I need from you is a summary of what the user asked for and decisions made, so that I can give the other agent appropriate context.]";

/// Builds the reviewer's opening prompt from the primary's summary and the
/// proposal exactly as the primary wrote it.
#[must_use]
pub fn review_request(summary: &str, proposal: &str) -> String {
    format!(
        "[HARNESS NOTE: another agent produced the plan below and its user wants a second opinion before approving it. Critique the plan: say what it gets right, what it gets wrong, and what it misses. Do not implement it and do not edit any files.]\n\n\
         <context_from_the_planning_agent>\n{summary}\n</context_from_the_planning_agent>\n\n\
         <plan_under_review>\n{proposal}\n</plan_under_review>"
    )
}

/// Wraps a reviewer's answer for the primary. The answer travels unchanged;
/// only the note around it is Hel's.
#[must_use]
pub fn transfer_note(answer: &str) -> String {
    format!(
        "[HARNESS NOTE: the user asked another agent to review your plan. Its review follows verbatim. Weigh it, then revise your plan. You are still in plan mode; do not start implementing.]\n\n\
         <second_opinion>\n{answer}\n</second_opinion>"
    )
}

/// Tells the primary to implement the plan it proposed, quoted back so the
/// instruction cannot drift from what the user approved.
#[must_use]
pub fn implement_original_note(proposal: &str) -> String {
    format!(
        "[HARNESS NOTE: the user reviewed a second opinion and chose to implement your original plan unchanged. Implement exactly the plan below.]\n\n\
         <approved_plan>\n{proposal}\n</approved_plan>"
    )
}

/// Work the review workflow needs the transport to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRequest {
    /// Send `prompt` to the primary session under `command_id`.
    PromptPrimary { command_id: String, prompt: String },
    /// Send `prompt` to the reviewer session under `command_id`.
    PromptReviewer { command_id: String, prompt: String },
    /// Cancel any reviewer turn in flight and pause its process, keeping its
    /// native session and transcript for the next review.
    PauseReviewer,
    /// Put a Hel-owned decision dialog back in front of the user. The harness's
    /// own approval was consumed to gather context, so only Hel can offer the
    /// captured proposal again.
    RestoreDecision {
        proposal_id: String,
        proposal: String,
    },
}

/// How far one review has got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ReviewStage {
    /// The primary was asked to summarize what led to the plan.
    GatheringContext { command_id: String },
    /// The reviewer is reading the plan.
    Reviewing { command_id: String },
    /// The reviewer's answer is complete and can be transferred.
    Answered { answer: String },
    /// The reviewer's answer went to the primary; the split is closing.
    Transferred,
    /// The primary was told to implement the captured plan.
    Implementing,
    /// The user backed out. Hel owes them a decision on the captured proposal.
    Cancelled,
}

/// One second-opinion review, from the decision that started it to the command
/// that ends it.
///
/// Every step names the command it is waiting on, and a completion for any
/// other command is ignored. A reconnect that replays an already-applied
/// completion therefore cannot send feedback twice or start a second reviewer
/// turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewWorkflow {
    proposal_id: String,
    proposal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    stage: ReviewStage,
}

impl ReviewWorkflow {
    /// Starts a review of `proposal`, asking the primary for context under
    /// `command_id`.
    ///
    /// The caller resolves the harness's own plan decision first, through the
    /// dialect bridge, so this prompt reaches an idle planning session.
    #[must_use]
    pub fn start(
        proposal_id: impl Into<String>,
        proposal: impl Into<String>,
        command_id: impl Into<String>,
    ) -> (Self, WorkflowRequest) {
        let command_id = command_id.into();
        let workflow = Self {
            proposal_id: proposal_id.into(),
            proposal: proposal.into(),
            summary: None,
            stage: ReviewStage::GatheringContext {
                command_id: command_id.clone(),
            },
        };
        let request = WorkflowRequest::PromptPrimary {
            command_id,
            prompt: PRIMARY_CONTEXT_REQUEST.to_owned(),
        };
        (workflow, request)
    }

    #[must_use]
    pub fn stage(&self) -> &ReviewStage {
        &self.stage
    }

    #[must_use]
    pub fn proposal(&self) -> &str {
        &self.proposal
    }

    #[must_use]
    pub fn proposal_id(&self) -> &str {
        &self.proposal_id
    }

    /// The primary's summary, once it has arrived.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Whether the split's transfer action is currently allowed. It is not
    /// until the reviewer's latest turn has a complete answer.
    #[must_use]
    pub fn can_transfer(&self) -> bool {
        matches!(self.stage, ReviewStage::Answered { .. })
    }

    /// Whether the workflow has ended and its split should close.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(
            self.stage,
            ReviewStage::Transferred | ReviewStage::Implementing | ReviewStage::Cancelled
        )
    }

    /// Records the primary's answer to the context request and prompts the
    /// reviewer under `reviewer_command_id`.
    ///
    /// A completion for any other command is ignored, so replaying an old
    /// completion after a reconnect starts no second reviewer turn.
    pub fn primary_context_completed(
        &mut self,
        command_id: &str,
        summary: impl Into<String>,
        reviewer_command_id: impl Into<String>,
    ) -> Option<WorkflowRequest> {
        let ReviewStage::GatheringContext {
            command_id: awaited,
        } = &self.stage
        else {
            return None;
        };
        if awaited != command_id {
            return None;
        }
        let summary = summary.into();
        let prompt = review_request(&summary, &self.proposal);
        let reviewer_command_id = reviewer_command_id.into();
        self.summary = Some(summary);
        self.stage = ReviewStage::Reviewing {
            command_id: reviewer_command_id.clone(),
        };
        Some(WorkflowRequest::PromptReviewer {
            command_id: reviewer_command_id,
            prompt,
        })
    }

    /// Records the reviewer's completed answer, which enables transfer.
    pub fn reviewer_turn_completed(&mut self, command_id: &str, answer: impl Into<String>) {
        let ReviewStage::Reviewing {
            command_id: awaited,
        } = &self.stage
        else {
            return;
        };
        if awaited != command_id {
            return;
        }
        self.stage = ReviewStage::Answered {
            answer: answer.into(),
        };
    }

    /// Sends the reviewer's latest answer to the primary, unchanged, under a
    /// harness note. Plan mode stays active: the revised plan is the primary's
    /// to produce.
    pub fn transfer(&mut self, command_id: impl Into<String>) -> Vec<WorkflowRequest> {
        let ReviewStage::Answered { answer } = &self.stage else {
            return Vec::new();
        };
        let prompt = transfer_note(answer);
        self.stage = ReviewStage::Transferred;
        vec![
            WorkflowRequest::PromptPrimary {
                command_id: command_id.into(),
                prompt,
            },
            WorkflowRequest::PauseReviewer,
        ]
    }

    /// Tells the primary to implement the captured proposal. No reviewer
    /// feedback travels with it.
    pub fn implement_original(&mut self, command_id: impl Into<String>) -> Vec<WorkflowRequest> {
        if self.finished() {
            return Vec::new();
        }
        let prompt = implement_original_note(&self.proposal);
        self.stage = ReviewStage::Implementing;
        vec![
            WorkflowRequest::PromptPrimary {
                command_id: command_id.into(),
                prompt,
            },
            WorkflowRequest::PauseReviewer,
        ]
    }

    /// Backs out without transferring anything, and asks for the captured
    /// proposal to be put back in front of the user.
    pub fn cancel(&mut self) -> Vec<WorkflowRequest> {
        if self.finished() {
            return Vec::new();
        }
        self.stage = ReviewStage::Cancelled;
        vec![
            WorkflowRequest::PauseReviewer,
            WorkflowRequest::RestoreDecision {
                proposal_id: self.proposal_id.clone(),
                proposal: self.proposal.clone(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
    };

    fn profiles() -> Vec<ReviewerProfileChoice> {
        vec![
            ReviewerProfileChoice {
                id: "codex".into(),
                harness: "codex".into(),
            },
            ReviewerProfileChoice {
                id: "claude".into(),
                harness: "claude".into(),
            },
        ]
    }

    fn select(
        id: &str,
        current: &str,
        values: &[&str],
        category: SessionConfigOptionCategory,
    ) -> SessionConfigOption {
        SessionConfigOption::select(
            id.to_owned(),
            id.to_owned(),
            current.to_owned(),
            SessionConfigSelectOptions::Ungrouped(
                values
                    .iter()
                    .map(|value| {
                        SessionConfigSelectOption::new((*value).to_owned(), (*value).to_owned())
                    })
                    .collect(),
            ),
        )
        .category(category)
    }

    fn model_option(values: &[&str]) -> SessionConfigOption {
        select(
            "model",
            values[0],
            values,
            SessionConfigOptionCategory::Model,
        )
    }

    fn effort_option(values: &[&str]) -> SessionConfigOption {
        select(
            "effort",
            values[0],
            values,
            SessionConfigOptionCategory::ThoughtLevel,
        )
    }

    fn setup() -> ReviewerSetup {
        ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default())
    }

    fn values(choices: &[SessionConfigChoice]) -> Vec<String> {
        choices.iter().map(|choice| choice.value.clone()).collect()
    }

    #[test]
    fn the_waterfall_walks_profile_then_model_then_effort() {
        let mut setup = setup();
        assert_eq!(setup.stage(), SetupStage::Profile);
        assert!(setup.models().is_empty());

        assert_eq!(
            setup.confirm(),
            SetupOutcome::Requests(vec![SetupRequest::Probe {
                generation: 1,
                profile_id: "codex".into(),
            }])
        );
        // Discovery is running, so nothing can be confirmed yet.
        assert_eq!(setup.stage(), SetupStage::Model);
        assert!(setup.busy());
        assert!(!setup.can_confirm());
        assert_eq!(setup.confirm(), SetupOutcome::None);

        assert!(
            setup
                .probe_succeeded(
                    1,
                    &[model_option(&["fast", "deep"]), effort_option(&["low"])]
                )
                .is_none()
        );
        assert!(!setup.busy());
        assert_eq!(values(setup.models()), vec!["fast", "deep"]);

        setup.move_selection(1);
        assert_eq!(
            setup.confirm(),
            SetupOutcome::Requests(vec![SetupRequest::ApplyModel {
                generation: 1,
                model: "deep".into(),
            }])
        );
        // The effort list is withheld until the refreshed options arrive.
        assert_eq!(setup.stage(), SetupStage::Effort);
        assert!(setup.busy());
        assert!(setup.efforts().is_empty());

        assert!(
            setup
                .model_applied(
                    1,
                    &[model_option(&["fast", "deep"]), effort_option(&["high"])]
                )
                .is_none()
        );
        assert_eq!(values(setup.efforts()), vec!["high"]);

        assert_eq!(
            setup.confirm(),
            SetupOutcome::Confirmed {
                selection: ReviewerSelection {
                    profile_id: "codex".into(),
                    model: Some("deep".into()),
                    effort: Some("high".into()),
                },
            }
        );
    }

    #[test]
    fn a_harness_with_no_selector_offers_a_single_harness_default() {
        let mut setup = setup();
        setup.confirm();
        setup.probe_succeeded(1, &[]);

        assert_eq!(values(setup.models()), vec![HARNESS_DEFAULT_VALUE]);
        assert_eq!(setup.models()[0].name, HARNESS_DEFAULT_LABEL);

        // Nothing is applied for a harness default, so no request goes out and
        // the efforts discovery already reported stay in place.
        assert_eq!(setup.confirm(), SetupOutcome::None);
        assert_eq!(setup.stage(), SetupStage::Effort);
        assert_eq!(values(setup.efforts()), vec![HARNESS_DEFAULT_VALUE]);

        assert_eq!(
            setup.confirm(),
            SetupOutcome::Confirmed {
                selection: ReviewerSelection {
                    profile_id: "codex".into(),
                    model: None,
                    effort: None,
                },
            }
        );
    }

    #[test]
    fn changing_the_profile_retires_the_probe_it_started() {
        let mut setup = setup();
        setup.confirm();
        setup.probe_failed(1, "harness did not start");
        assert_eq!(setup.failure(), Some("harness did not start"));
        assert!(!setup.can_confirm());

        assert_eq!(
            setup.retry(),
            SetupOutcome::Requests(vec![
                SetupRequest::CancelProbe { generation: 1 },
                SetupRequest::Probe {
                    generation: 2,
                    profile_id: "codex".into(),
                },
            ])
        );

        setup.probe_succeeded(2, &[model_option(&["fast"])]);
        assert_eq!(
            setup.back(),
            SetupOutcome::Requests(vec![SetupRequest::CancelProbe { generation: 2 }])
        );
        assert_eq!(setup.stage(), SetupStage::Profile);

        setup.move_selection(1);
        assert_eq!(
            setup.confirm(),
            SetupOutcome::Requests(vec![SetupRequest::Probe {
                generation: 3,
                profile_id: "claude".into(),
            }])
        );
    }

    #[test]
    fn a_result_from_a_retired_probe_is_dropped_and_its_reviewer_stopped() {
        let mut setup = setup();
        setup.confirm();
        setup.probe_succeeded(1, &[model_option(&["fast"])]);
        setup.back();
        setup.move_selection(1);
        setup.confirm();

        // The first profile's harness answers late. It must not repopulate the
        // list the second profile is now discovering.
        assert_eq!(
            setup.probe_succeeded(1, &[model_option(&["stale"])]),
            Some(SetupRequest::CancelProbe { generation: 1 })
        );
        assert!(setup.busy());
        assert!(setup.models().is_empty());

        setup.probe_succeeded(2, &[model_option(&["current"])]);
        assert_eq!(values(setup.models()), vec!["current"]);
    }

    #[test]
    fn a_late_failure_from_a_retired_probe_never_blocks_the_current_one() {
        let mut setup = setup();
        setup.confirm();
        setup.probe_succeeded(1, &[model_option(&["fast"])]);
        setup.back();
        setup.confirm();

        setup.probe_failed(1, "the abandoned harness died");
        assert_eq!(setup.failure(), None);
        assert!(setup.busy());
    }

    #[test]
    fn cancelling_names_every_reviewer_the_caller_must_stop() {
        let mut setup = setup();
        setup.confirm();
        assert_eq!(
            setup.cancel(),
            SetupOutcome::Cancelled {
                requests: vec![SetupRequest::CancelProbe { generation: 1 }],
            }
        );
    }

    #[test]
    fn confirming_keeps_the_reviewer_it_chose_running() {
        let mut setup = setup();
        setup.confirm();
        setup.probe_failed(1, "restarting");
        // Retrying stops the failed reviewer as it starts the replacement.
        assert_eq!(
            setup.retry(),
            SetupOutcome::Requests(vec![
                SetupRequest::CancelProbe { generation: 1 },
                SetupRequest::Probe {
                    generation: 2,
                    profile_id: "codex".into(),
                },
            ])
        );
        setup.probe_succeeded(2, &[]);
        setup.confirm();
        setup.confirm();

        // Generation 2 is handed to the review, so cancelling now stops
        // nothing: the setup no longer owns a provisional reviewer.
        assert_eq!(
            setup.cancel(),
            SetupOutcome::Cancelled {
                requests: Vec::new()
            }
        );
    }

    #[test]
    fn a_workspace_reopens_on_what_it_confirmed_last() {
        let mut first = setup();
        first.confirm();
        first.probe_succeeded(1, &[model_option(&["fast", "deep"])]);
        first.move_selection(1);
        first.confirm();
        first.model_applied(
            1,
            &[model_option(&["fast", "deep"]), effort_option(&["a", "b"])],
        );
        first.move_selection(1);
        first.confirm();

        let defaults = first.defaults().clone();
        assert_eq!(defaults.profile("workspace-1"), Some("codex"));
        assert_eq!(defaults.model("workspace-1", "codex"), Some("deep"));
        assert_eq!(defaults.effort("workspace-1", "codex", "deep"), Some("b"));
        // Another workspace shares nothing.
        assert_eq!(defaults.profile("workspace-2"), None);

        let mut second = ReviewerSetup::new("workspace-1", profiles(), defaults);
        assert_eq!(second.profile_index(), 0);
        second.confirm();
        second.probe_succeeded(1, &[model_option(&["fast", "deep"])]);
        assert_eq!(second.model_index(), 1);
        second.confirm();
        second.model_applied(
            1,
            &[model_option(&["fast", "deep"]), effort_option(&["a", "b"])],
        );
        assert_eq!(second.effort_index(), 1);
    }

    #[test]
    fn a_remembered_value_the_harness_no_longer_advertises_falls_back() {
        let mut defaults = ReviewerDefaults::default();
        defaults.remember(
            "workspace-1",
            &ReviewerSelection {
                profile_id: "retired".into(),
                model: Some("gone".into()),
                effort: Some("vanished".into()),
            },
        );

        let mut setup = ReviewerSetup::new("workspace-1", profiles(), defaults);
        // The remembered profile is no longer configured.
        assert_eq!(setup.profile_index(), 0);

        setup.confirm();
        setup.probe_succeeded(1, &[model_option(&["fast", "deep"])]);
        assert_eq!(setup.model_index(), 0);
        setup.confirm();
        setup.model_applied(
            1,
            &[model_option(&["fast", "deep"]), effort_option(&["a", "b"])],
        );
        assert_eq!(setup.effort_index(), 0);
    }

    #[test]
    fn stepping_back_is_refused_while_a_probe_is_running() {
        let mut setup = setup();
        setup.confirm();
        assert_eq!(setup.back(), SetupOutcome::None);
        assert_eq!(setup.stage(), SetupStage::Model);
    }

    fn started() -> (ReviewWorkflow, WorkflowRequest) {
        ReviewWorkflow::start("plan-review-1", "1. Read\n2. Change", "context-1")
    }

    #[test]
    fn a_review_asks_the_primary_for_context_before_it_prompts_the_reviewer() {
        let (mut workflow, request) = started();
        assert_eq!(
            request,
            WorkflowRequest::PromptPrimary {
                command_id: "context-1".into(),
                prompt: PRIMARY_CONTEXT_REQUEST.to_owned(),
            }
        );
        // The captured plan travels with the workflow, so the primary is never
        // asked to repeat it.
        assert!(PRIMARY_CONTEXT_REQUEST.contains("you do not need to repeat that"));
        assert!(!workflow.can_transfer());

        let prompt = workflow
            .primary_context_completed("context-1", "The user asked for X", "review-1")
            .expect("the summary starts the reviewer's turn");
        let WorkflowRequest::PromptReviewer { command_id, prompt } = prompt else {
            panic!("the reviewer is prompted next");
        };
        assert_eq!(command_id, "review-1");
        assert!(prompt.contains("The user asked for X"));
        // The proposal reaches the reviewer exactly as the primary wrote it.
        assert!(prompt.contains("1. Read\n2. Change"));
        assert!(prompt.contains("Do not implement it"));
        assert_eq!(workflow.summary(), Some("The user asked for X"));
        assert!(!workflow.can_transfer());
    }

    #[test]
    fn transfer_waits_for_a_complete_reviewer_answer() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        assert!(!workflow.can_transfer());
        assert!(workflow.transfer("transfer-1").is_empty());

        workflow.reviewer_turn_completed("review-1", "The plan misses error handling.");
        assert!(workflow.can_transfer());

        let requests = workflow.transfer("transfer-1");
        let [
            WorkflowRequest::PromptPrimary { command_id, prompt },
            WorkflowRequest::PauseReviewer,
        ] = requests.as_slice()
        else {
            panic!("transfer prompts the primary and pauses the reviewer");
        };
        assert_eq!(command_id, "transfer-1");
        // The answer travels unchanged inside Hel's note.
        assert!(prompt.contains("The plan misses error handling."));
        assert!(prompt.contains("do not start implementing"));
        assert!(workflow.finished());
    }

    #[test]
    fn a_replayed_completion_never_sends_feedback_twice() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        // The same context completion arriving again after a reconnect must
        // not start a second reviewer turn.
        assert!(
            workflow
                .primary_context_completed("context-1", "context", "review-2")
                .is_none()
        );
        assert_eq!(
            workflow.stage(),
            &ReviewStage::Reviewing {
                command_id: "review-1".into()
            }
        );

        workflow.reviewer_turn_completed("review-1", "answer");
        assert!(!workflow.transfer("transfer-1").is_empty());
        assert!(workflow.transfer("transfer-2").is_empty());
        assert!(workflow.implement_original("implement-1").is_empty());
        assert!(workflow.cancel().is_empty());
    }

    #[test]
    fn a_completion_for_another_command_is_ignored() {
        let (mut workflow, _) = started();
        assert!(
            workflow
                .primary_context_completed("some-other-command", "stale", "review-1")
                .is_none()
        );
        assert_eq!(workflow.summary(), None);

        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("a-different-turn", "stale answer");
        assert!(!workflow.can_transfer());
    }

    #[test]
    fn implementing_the_original_carries_the_captured_plan_and_no_feedback() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("review-1", "I would restructure the whole thing.");

        let requests = workflow.implement_original("implement-1");
        let [
            WorkflowRequest::PromptPrimary { prompt, .. },
            WorkflowRequest::PauseReviewer,
        ] = requests.as_slice()
        else {
            panic!("implementing prompts the primary and pauses the reviewer");
        };
        assert!(prompt.contains("1. Read\n2. Change"));
        assert!(!prompt.contains("I would restructure the whole thing."));
        assert!(workflow.finished());
    }

    #[test]
    fn cancelling_transfers_nothing_and_asks_for_the_decision_back() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("review-1", "unwanted feedback");

        assert_eq!(
            workflow.cancel(),
            vec![
                WorkflowRequest::PauseReviewer,
                WorkflowRequest::RestoreDecision {
                    proposal_id: "plan-review-1".into(),
                    proposal: "1. Read\n2. Change".into(),
                },
            ]
        );
        assert!(workflow.finished());
    }

    #[test]
    fn remembered_defaults_survive_a_serialization_round_trip() {
        let mut defaults = ReviewerDefaults::default();
        defaults.remember(
            "workspace-1",
            &ReviewerSelection {
                profile_id: "codex".into(),
                model: None,
                effort: Some("high".into()),
            },
        );
        let encoded = serde_json::to_string(&defaults).unwrap();
        let decoded: ReviewerDefaults = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, defaults);
        // A harness default round-trips as the same preselected row.
        assert_eq!(
            decoded.model("workspace-1", "codex"),
            Some(HARNESS_DEFAULT_VALUE)
        );
    }
}
