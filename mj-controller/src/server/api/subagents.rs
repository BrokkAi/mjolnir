use crate::profile_scheduler::draw_candidates;
#[cfg(test)]
use crate::profile_scheduler::{order_by_draw, rank_candidates};
use mj_core::model_catalog;

use super::*;

pub(super) const MAX_SUBAGENT_CONTEXT_BYTES: usize = 256 * 1024;

pub(super) async fn spawn_subagent(
    State(state): State<ServerState>,
    Path(parent_session_id): Path<String>,
    Json(request): Json<SpawnSubagentRequest>,
) -> Result<(StatusCode, Json<SubagentView>), ApiFailure> {
    let backend = backend(&state)?.clone();
    let parent = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &parent_session_id)?.clone()
    };
    let parent_harness = parent
        .harness_kind
        .parse::<mj_core::config::HarnessKind>()
        .map_err(|error| ApiFailure::conflict(format!("{error:#}")))?;
    if !parent_harness.supports_delegation_tools() {
        return Err(ApiFailure::conflict(format!(
            "{} sessions cannot spawn Mjolnir sub-agents",
            parent_harness.display_name()
        )));
    }
    let initial_prompt = build_subagent_prompt(&request.instructions)?;
    if request.task_name.trim().is_empty() {
        return Err(ApiFailure::bad_request("task_name cannot be empty"));
    }
    let selection = resolve_subagent_policy_selection(
        &backend,
        &parent.profile_id,
        &parent.subagents,
        request.task_type.unwrap_or_default(),
    )
    .await?;

    let relation = backend
        .start_subagent(crate::controller::RegisterSubagentRequest {
            no_github_credentials: request.no_github_credentials,
            kind: mj_core::subagent::SubagentKind::Worker,
            reviewer_launch: None,
            parent_session_id: parent_session_id.clone(),
            task_name: request.task_name,
            profile_id: selection.profile_id,
            model: Some(selection.model.clone()),
            effort: selection.effort.clone(),
            fast_mode: selection.fast_mode,
            working_directory: request.working_directory.unwrap_or_default(),
            initial_prompt,
            // Every request is its own spawn; the key only lets Mjolnir
            // recognise one request it is asked to run twice.
            request_key: mj_core::state::new_session_id().map_err(ApiFailure::from)?,
            // The daemon creates the report root when it registers the child.
            report_root: None,
        })
        .await
        .map_err(|error| ApiFailure::conflict(format!("sub-agent creation failed: {error:#}")))?;
    let session = await_session_record(&state, &relation.child_session_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(SubagentView {
            parent_session_id,
            task_name: relation.task_name,
            session,
        }),
    ))
}

/// The profile, model and effort a spawn runs its child with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentSelection {
    pub profile_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub fast_mode: bool,
}

#[derive(Debug, Clone)]
struct ModelCandidate {
    candidate: SubagentCandidate,
    model: model_catalog::ModelOffer,
}

impl AsRef<SubagentCandidate> for ModelCandidate {
    fn as_ref(&self) -> &SubagentCandidate {
        &self.candidate
    }
}

impl std::ops::Deref for ModelCandidate {
    type Target = SubagentCandidate;
    fn deref(&self) -> &Self::Target {
        &self.candidate
    }
}

/// Resolve the session override or global task choice, drawing a profile by quota.
pub(crate) async fn resolve_subagent_policy_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    policy: &mj_core::subagent::SubagentPolicy,
    task_type: mj_core::subagent::SubagentTaskType,
) -> Result<SubagentSelection, ApiFailure> {
    if task_type == mj_core::subagent::SubagentTaskType::Oracle {
        return Err(ApiFailure::bad_request(
            "task_type `oracle` is reserved for the read-only oracle and cannot be spawned as a sub-agent",
        ));
    }
    if !policy.uses_mjolnir() {
        return Err(ApiFailure::conflict(
            "this session does not allow Mjolnir sub-agents",
        ));
    }
    let config = backend.subagent_config().await?;
    let choice = config.effective_choice(policy, task_type).ok_or_else(|| {
        // A parent launched while subtle was configured can still ask for it.
        if task_type == mj_core::subagent::SubagentTaskType::Subtle
            && config
                .effective_choice(policy, mj_core::subagent::SubagentTaskType::Straightforward)
                .is_some()
        {
            ApiFailure::conflict(
                "The user configured only the straightforward task type; spawn this child with task_type `straightforward`",
            )
        } else {
            ApiFailure::conflict(format!(
                "Configure [subagents.{}] or pass --subagent-{} <MODEL> before spawning this Mjolnir sub-agent",
                task_type.id(),
                task_type.id()
            ))
        }
    })?;
    if choice.effort.as_deref() == Some("adaptive") {
        return Err(ApiFailure::bad_request(
            "adaptive sub-agent effort is not supported",
        ));
    }
    resolve_model_profile_selection_matching(
        backend,
        None,
        &choice.model,
        choice
            .effort
            .as_deref()
            .map(EffortRequirement::Exact)
            .unwrap_or(EffortRequirement::Default),
        None,
        ModelProfileCandidateSource::Subagents { parent_profile },
    )
    .await
    .map_err(|error| {
        let context = if policy.override_choice(task_type).is_some() {
            format!("session --subagent-{} override", task_type.id())
        } else {
            format!("[subagents.{}]", task_type.id())
        };
        ApiFailure::conflict(format!("{context}: {}", error.message))
    })
}

/// The profile and effort for the oracle's model. Like a spawn, it ranks the
/// enabled profiles that offer the model by quota.
pub(crate) async fn resolve_oracle_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    choice: &mj_core::config::SubagentChoice,
) -> Result<SubagentSelection, ApiFailure> {
    resolve_model_profile_selection_matching(
        backend,
        None,
        &choice.model,
        choice
            .effort
            .as_deref()
            .map(EffortRequirement::Exact)
            .unwrap_or(EffortRequirement::Default),
        None,
        ModelProfileCandidateSource::Subagents { parent_profile },
    )
    .await
}

/// Choose a profile for a user-created session from all enabled, usable
/// profiles and validate its effort.
pub(crate) async fn resolve_session_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    ranking_anchor: Option<&str>,
    model: &str,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    if effort == Some("adaptive") {
        return Err(ApiFailure::bad_request(
            "adaptive effort is no longer supported",
        ));
    }
    let requirement = effort
        .map(EffortRequirement::Exact)
        .unwrap_or(EffortRequirement::Any);
    resolve_model_profile_selection_matching(
        backend,
        None,
        model,
        requirement,
        None,
        ModelProfileCandidateSource::Session { ranking_anchor },
    )
    .await
}

#[derive(Clone, Copy)]
enum ModelProfileCandidateSource<'a> {
    Session { ranking_anchor: Option<&'a str> },
    Subagents { parent_profile: &'a str },
    Reviewers,
    ReviewersReady,
}

impl<'a> ModelProfileCandidateSource<'a> {
    fn profile_description(self) -> &'static str {
        match self {
            Self::Session { .. } => "configured",
            Self::Subagents { .. } => "enabled",
            Self::Reviewers | Self::ReviewersReady => "enabled",
        }
    }
}

#[derive(Clone, Copy)]
enum EffortRequirement<'a> {
    Any,
    Default,
    Exact(&'a str),
}

/// A reviewer uses the same enabled-profile chooser and effort validation as spawn.
pub(crate) async fn resolve_reviewer_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    model: &str,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let requirement = effort
        .map(EffortRequirement::Exact)
        .unwrap_or(EffortRequirement::Default);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        if let Ok(selected) = resolve_model_profile_selection_matching(
            backend,
            None,
            model,
            requirement,
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        {
            return Ok(selected);
        }
        // A profile that offers the model but cannot serve this slot does not
        // end the search: later-discovered profiles may.
        let mut tried = Vec::new();
        let mut last_error = None;
        loop {
            let candidates = match backend
                .reviewer_candidates_for_model(model.to_owned(), tried.clone())
                .await
            {
                Ok(candidates) => candidates,
                Err(error) => return Err(last_error.unwrap_or_else(|| error.into())),
            };
            let before = tried.len();
            for id in candidates
                .offered
                .iter()
                .map(|candidate| &candidate.profile_id)
                .chain(candidates.unavailable.iter().map(|(id, _)| id))
            {
                if !tried.contains(id) {
                    tried.push(id.clone());
                }
            }
            if tried.len() == before {
                return Err(last_error.unwrap_or_else(|| {
                    ApiFailure::unavailable(format!("no reviewer profile offers model {model:?}"))
                }));
            }
            match resolve_model_profile_selection_from(
                backend,
                candidates,
                None,
                model,
                requirement,
                None,
                ModelProfileCandidateSource::Reviewers,
            )
            .await
            {
                Ok(selected) => return Ok(selected),
                Err(error) => last_error = Some(error),
            }
        }
    })
    .await
    .map_err(|_| {
        ApiFailure::unavailable(format!(
            "reviewer effort discovery timed out for model {model:?}"
        ))
    })?
}

pub(crate) async fn resolve_pinned_reviewer_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    profile: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let choices = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.profile_config(profile.to_owned(), Some(model.to_owned()), false),
    )
    .await
    .map_err(|_| {
        ApiFailure::unavailable(format!(
            "review profile {profile:?} effort discovery timed out"
        ))
    })?
    .map_err(|error| {
        ApiFailure::unavailable(format!(
            "review profile {profile:?} is unavailable: {error:#}"
        ))
    })?;
    validate_selectors(&choices, Some(model), effort)?;
    Ok(selection(
        profile.to_owned(),
        model,
        effort.map(str::to_owned),
    ))
}

async fn resolve_model_profile_selection_matching(
    backend: &Arc<dyn SubagentBackend>,
    profile_id: Option<&str>,
    model: &str,
    requirement: EffortRequirement<'_>,
    inherited_effort: Option<&str>,
    source: ModelProfileCandidateSource<'_>,
) -> Result<SubagentSelection, ApiFailure> {
    let candidates = match source {
        ModelProfileCandidateSource::Session { .. } => backend.session_profile_candidates().await?,
        ModelProfileCandidateSource::Reviewers => {
            backend
                .reviewer_candidates_for_model(model.to_owned(), Vec::new())
                .await?
        }
        ModelProfileCandidateSource::ReviewersReady => {
            backend.reviewer_ready_candidates(model.to_owned()).await?
        }
        ModelProfileCandidateSource::Subagents { parent_profile } => {
            backend
                .subagent_candidates(parent_profile.to_owned())
                .await?
        }
    };
    resolve_model_profile_selection_from(
        backend,
        candidates,
        profile_id,
        model,
        requirement,
        inherited_effort,
        source,
    )
    .await
}

async fn resolve_model_profile_selection_from(
    backend: &Arc<dyn SubagentBackend>,
    candidates: SubagentCandidates,
    profile_id: Option<&str>,
    model: &str,
    requirement: EffortRequirement<'_>,
    inherited_effort: Option<&str>,
    source: ModelProfileCandidateSource<'_>,
) -> Result<SubagentSelection, ApiFailure> {
    let SubagentCandidates {
        mut offered,
        unavailable,
    } = candidates;
    if let Some(requested) = profile_id {
        if let Some((_, reason)) = unavailable.iter().find(|(id, _)| id == requested) {
            return Err(ApiFailure::conflict(format!(
                "profile {requested:?} is unavailable: {reason}"
            )));
        }
        offered.retain(|candidate| candidate.profile_id == requested);
        if offered.is_empty() {
            let message = match source {
                ModelProfileCandidateSource::Session { .. } => {
                    format!("profile {requested:?} is not available for a new session")
                }
                ModelProfileCandidateSource::Subagents { .. } => format!(
                    "profile {requested:?} is unavailable for sub-agent use from this session"
                ),
                ModelProfileCandidateSource::Reviewers
                | ModelProfileCandidateSource::ReviewersReady => {
                    format!("profile {requested:?} is unavailable for reviewer use")
                }
            };
            return Err(ApiFailure::bad_request(message));
        }
    }
    let all_offered = offered.clone();
    let models = offered
        .iter()
        .flat_map(|candidate| candidate.choices.models.iter().cloned())
        .collect::<Vec<_>>();
    let identity = model_catalog::model_id(&models, model);
    let mut offered = offered
        .into_iter()
        .filter_map(|candidate| {
            model_catalog::offer(&candidate.choices.models, &identity, model)
                .map(|model| ModelCandidate { candidate, model })
        })
        .collect::<Vec<_>>();
    if offered.is_empty() {
        if let Some(candidate) = all_offered.first().filter(|_| profile_id.is_some()) {
            validate_selectors(&candidate.choices, Some(model), None)?;
        }
        return Err(ApiFailure::bad_request(no_profile_offers(
            model,
            &all_offered,
            &unavailable,
            source.profile_description(),
        )));
    }
    if profile_id.is_none() {
        let prefer = backend.quota_preferences().await?;
        let anchor = match source {
            ModelProfileCandidateSource::Subagents { parent_profile } => Some(parent_profile),
            ModelProfileCandidateSource::Session { ranking_anchor } => ranking_anchor,
            ModelProfileCandidateSource::Reviewers
            | ModelProfileCandidateSource::ReviewersReady => None,
        };
        draw_candidates(&mut offered, anchor, &prefer, &identity, now_epoch_seconds())?;
    }
    let reviewer_source = matches!(
        source,
        ModelProfileCandidateSource::Reviewers | ModelProfileCandidateSource::ReviewersReady
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut probes = tokio::task::JoinSet::new();
    if reviewer_source && !matches!(requirement, EffortRequirement::Default) {
        for candidate in &offered {
            let backend = Arc::clone(backend);
            let candidate = candidate.clone();
            probes.spawn(async move {
                let result = if candidate.choices.model.as_deref() == Some(&candidate.model.value)
                    && !candidate.choices.efforts.is_empty()
                {
                    Ok(candidate.choices.efforts.clone())
                } else {
                    model_efforts(&backend, &candidate.candidate, &candidate.model.value).await
                };
                (candidate.candidate.profile_id, result)
            });
        }
    }
    let mut ready_results: std::collections::HashMap<
        String,
        Result<Vec<mj_core::acp::SessionConfigChoice>, ApiFailure>,
    > = std::collections::HashMap::new();
    // The grace starts when a later candidate can serve the slot. It belongs
    // to the slot, not to each earlier candidate in the draw order.
    let mut later_ready_deadline: Option<tokio::time::Instant> = None;
    let mut offered_any_effort = false;
    let mut selection_errors = Vec::new();
    let mut unhealthy = Vec::new();
    for (index, candidate) in offered.iter().enumerate() {
        // A pinned profile runs even while marked: someone chose it. Other
        // profiles must be skipped before probing a login that just failed.
        if profile_id.is_none()
            && let Some(note) = unhealthy_note(&candidate.candidate, &candidate.model.value)
        {
            unhealthy.push(note);
            continue;
        }
        let result = if reviewer_source && matches!(requirement, EffortRequirement::Default) {
            Ok(Vec::new())
        } else if reviewer_source {
            let mut candidate_deadline = deadline;
            loop {
                if let Some(result) = ready_results.remove(&candidate.profile_id) {
                    break result;
                }
                let later_ready = offered.iter().skip(index + 1).any(|later| {
                    unhealthy_note(&later.candidate, &later.model.value).is_none()
                        && ready_results.get(&later.profile_id).is_some_and(|result| {
                            result.as_ref().is_ok_and(|efforts| match requirement {
                                EffortRequirement::Exact(requested) => {
                                    efforts.iter().any(|effort| effort.value == requested)
                                }
                                _ => false,
                            })
                        })
                });
                if later_ready {
                    let grace = *later_ready_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + std::time::Duration::from_secs(2)
                    });
                    candidate_deadline = candidate_deadline.min(grace);
                }
                match tokio::time::timeout_at(candidate_deadline, probes.join_next()).await {
                    Ok(Some(Ok((id, result)))) => {
                        let later_qualifies = offered.iter().skip(index + 1).any(|later| {
                            later.profile_id == id
                                && unhealthy_note(&later.candidate, &later.model.value).is_none()
                        }) && result.as_ref().is_ok_and(|efforts| {
                            match requirement {
                                EffortRequirement::Exact(requested) => {
                                    efforts.iter().any(|effort| effort.value == requested)
                                }
                                _ => false,
                            }
                        });
                        ready_results.insert(id, result);
                        if later_qualifies {
                            let grace = *later_ready_deadline.get_or_insert_with(|| {
                                tokio::time::Instant::now() + std::time::Duration::from_secs(2)
                            });
                            candidate_deadline = candidate_deadline.min(grace);
                        }
                    }
                    Ok(Some(Err(error))) => {
                        selection_errors.push(format!("reviewer probe failed: {error}"))
                    }
                    Ok(None) | Err(_) => {
                        break Err(ApiFailure::unavailable(format!(
                            "reviewer effort probe timed out for {:?}",
                            candidate.profile_id
                        )));
                    }
                }
            }
        } else {
            model_efforts(backend, &candidate.candidate, &candidate.model.value).await
        };
        let efforts = match result {
            Ok(efforts) => efforts,
            Err(error) => {
                selection_errors.push(format!("{}: {error:#}", candidate.profile_id));
                continue;
            }
        };
        offered_any_effort |= !efforts.is_empty();
        let effort = match requirement {
            EffortRequirement::Exact(requested)
                if efforts.iter().any(|choice| choice.value == requested) =>
            {
                Some(requested.to_owned())
            }
            EffortRequirement::Default => None,
            EffortRequirement::Exact(_) => {
                selection_errors.push(format!(
                    "{} offers efforts: {}",
                    candidate.profile_id,
                    effort_names(&efforts)
                ));
                continue;
            }
            EffortRequirement::Any => {
                child_effort(&candidate.model.value, &efforts, None, inherited_effort)?
            }
        };
        probes.abort_all();
        return Ok(selection(
            candidate.profile_id.clone(),
            &candidate.model.value,
            effort,
        ));
    }
    probes.abort_all();
    if !unhealthy.is_empty() {
        return Err(all_unhealthy(
            model,
            &unhealthy,
            source.profile_description(),
        ));
    }
    let required = match requirement {
        EffortRequirement::Exact(effort) => format!("effort {effort:?}"),
        EffortRequirement::Default => "harness default effort".to_owned(),
        EffortRequirement::Any => "an available effort".to_owned(),
    };
    let unavailable_effort = match requirement {
        EffortRequirement::Exact(effort) if !offered_any_effort => format!(
            "model {model:?} offers no effort choices; requested effort {effort:?} is unavailable. "
        ),
        _ => format!(
            "no {} profile offers model {model:?} with {required}. ",
            source.profile_description()
        ),
    };
    Err(ApiFailure::bad_request(format!(
        "{unavailable_effort}{}",
        selection_errors
            .into_iter()
            .chain(
                unavailable
                    .iter()
                    .map(|(id, reason)| format!("{id}: {reason}")),
            )
            .collect::<Vec<_>>()
            .join("; ")
    )))
}

async fn model_efforts(
    backend: &Arc<dyn SubagentBackend>,
    candidate: &SubagentCandidate,
    model: &str,
) -> Result<Vec<mj_core::acp::SessionConfigChoice>, ApiFailure> {
    if candidate.choices.model.as_deref() == Some(model) && !candidate.choices.efforts.is_empty() {
        Ok(candidate.choices.efforts.clone())
    } else {
        backend
            .profile_config(candidate.profile_id.clone(), Some(model.to_owned()), false)
            .await
            .map(|choices| choices.efforts)
            .map_err(|error| {
                ApiFailure::unavailable(format!(
                    "could not read the efforts profile {:?} offers for model {model:?}: {error:#}",
                    candidate.profile_id
                ))
            })
    }
}

fn selection(profile_id: String, model: &str, effort: Option<String>) -> SubagentSelection {
    SubagentSelection {
        profile_id,
        model: model.to_owned(),
        effort,
        fast_mode: mj_core::codex_catalog::is_luna_model(model),
    }
}

fn effort_names(efforts: &[mj_core::acp::SessionConfigChoice]) -> String {
    if efforts.is_empty() {
        "none".to_owned()
    } else {
        efforts
            .iter()
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The effort a child starts with, from the efforts its model offers. A
/// requested effort must be one of them; a model that offers none takes no
/// effort at all. An omitted effort follows the parent's only when the
/// child's model offers it, so a parent at `high` can delegate to a model
/// without efforts.
fn child_effort(
    model: &str,
    offered: &[mj_core::acp::SessionConfigChoice],
    requested: Option<&str>,
    parent: Option<&str>,
) -> Result<Option<String>, ApiFailure> {
    let offers = |effort: &str| offered.iter().any(|choice| choice.value == effort);
    match requested {
        Some("adaptive") => Err(ApiFailure::bad_request(
            "adaptive effort is no longer supported",
        )),
        Some(effort) if offered.is_empty() => Err(ApiFailure::bad_request(format!(
            "model {model:?} offers no effort choices; spawn it without effort, not {effort:?}"
        ))),
        Some(effort) if !offers(effort) => Err(ApiFailure::bad_request(format!(
            "model {model:?} does not offer {effort:?} as effort; choices: {}",
            offered
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        Some(effort) => Ok(Some(effort.to_owned())),
        None => Ok(parent
            .filter(|effort| *effort != "adaptive" && offers(effort))
            .map(str::to_owned)),
    }
}

/// "profile codex2 is rate-limited until 14:05:00 UTC (...)" when the
/// candidate's sessions just failed running `model`.
fn unhealthy_note(candidate: &SubagentCandidate, model: &str) -> Option<String> {
    candidate
        .unhealthy
        .get(model)
        .map(|mark| format!("profile {} is {}", candidate.profile_id, mark.describe()))
}

/// The refusal when every profile that could run `model` is marked. It is a
/// conflict, not a bad request: the same spawn succeeds once a mark ends.
fn all_unhealthy(model: &str, notes: &[String], profile_description: &str) -> ApiFailure {
    ApiFailure::conflict(format!(
        "every {profile_description} profile that can run model {model:?} failed at the \
         provider just now: {}. Try again after that time.",
        notes.join("; ")
    ))
}

fn now_epoch_seconds() -> i64 {
    mj_core::clock::epoch_millis() / 1000
}

/// The candidates `list_profiles` shows: ranked, with profiles of one harness
/// that offer exactly the same models merged into the best-ranked of them.
/// Several logins of one account type become one entry, and a profile that
/// offers different models is never hidden behind another.
#[cfg(test)]
pub(crate) fn merge_same_models(
    mut candidates: Vec<SubagentCandidate>,
    parent_profile: &str,
    model: &str,
    now_seconds: i64,
) -> Vec<SubagentCandidate> {
    rank_candidates(&mut candidates, Some(parent_profile), &[], model, now_seconds);
    let mut seen = std::collections::BTreeSet::new();
    candidates.retain(|candidate| {
        let mut models = candidate
            .choices
            .models
            .iter()
            .map(|choice| choice.value.clone())
            .collect::<Vec<_>>();
        models.sort_unstable();
        seen.insert((candidate.harness, models))
    });
    candidates
}

/// A refusal that lists the models each candidate offers and profiles that
/// could not be checked.
fn no_profile_offers(
    model: &str,
    offered: &[SubagentCandidate],
    unavailable: &[(String, String)],
    profile_description: &str,
) -> String {
    let mut message = format!("no {profile_description} profile offers model {model:?}.");
    if !offered.is_empty() {
        let offers = offered
            .iter()
            .map(|candidate| {
                let models = candidate
                    .choices
                    .models
                    .iter()
                    .map(|choice| choice.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} ({models})", candidate.profile_id)
            })
            .collect::<Vec<_>>()
            .join("; ");
        message.push_str(&format!(" Offered: {offers}."));
    }
    if !unavailable.is_empty() {
        let skipped = unavailable
            .iter()
            .map(|(id, reason)| format!("{id} ({reason})"))
            .collect::<Vec<_>>()
            .join("; ");
        message.push_str(&format!(" Could not check: {skipped}."));
    }
    message
}

/// How long a just-created session is waited for in the viewer snapshot.
///
/// The snapshot is republished on a tick, so a child registered a moment ago
/// is usually not in it yet. Answering "unknown session" for a spawn that
/// succeeded tells the caller its child does not exist while that child is
/// starting, and invites it to spawn a second one.
const SNAPSHOT_CATCH_UP: std::time::Duration = std::time::Duration::from_secs(10);

async fn await_session_record(
    state: &ServerState,
    session_id: &str,
) -> Result<ApiSession, ApiFailure> {
    let mut snapshot_rx = state.snapshot_rx.clone();
    let deadline = tokio::time::Instant::now() + SNAPSHOT_CATCH_UP;
    loop {
        // The borrow ends before the await: a watch guard may not be held
        // across one, and holding it would block every other reader.
        let found = {
            let snapshot = snapshot_rx.borrow();
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .map(ApiSession::from)
        };
        if let Some(session) = found {
            return Ok(session);
        }
        if tokio::time::timeout_at(deadline, snapshot_rx.changed())
            .await
            .is_err()
        {
            let snapshot = snapshot_rx.borrow();
            return Ok(ApiSession::from(require_session_record(
                &snapshot, session_id,
            )?));
        }
    }
}

pub(super) async fn list_subagents(
    State(state): State<ServerState>,
    Path(parent_session_id): Path<String>,
) -> Result<Json<SubagentListResponse>, ApiFailure> {
    {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &parent_session_id)?;
    }
    let records = backend(&state)?
        .list_subagents(parent_session_id.clone())
        .await?;
    let snapshot = state.snapshot_rx.borrow();
    let subagents = records
        .into_iter()
        .filter(|record| record.kind != mj_core::subagent::SubagentKind::Reviewer)
        .map(|record| {
            let session = require_session_record(&snapshot, &record.child_session_id)?;
            Ok(SubagentView {
                parent_session_id: parent_session_id.clone(),
                task_name: record.task_name,
                session: ApiSession::from(session),
            })
        })
        .collect::<Result<Vec<_>, ApiFailure>>()?;
    Ok(Json(SubagentListResponse { subagents }))
}

/// New assignments have one text input; children read their own source files.
pub(crate) fn build_subagent_prompt(instructions: &str) -> Result<String, ApiFailure> {
    let prompt = instructions.trim();
    validate_prompt_text(prompt, false)?;
    if prompt.len() > MAX_SUBAGENT_CONTEXT_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "sub-agent handoff exceeds the {MAX_SUBAGENT_CONTEXT_BYTES}-byte limit"
        )));
    }
    Ok(prompt.to_owned())
}

/// Compatibility for accepted requests from workers running an older build.
pub(crate) struct LegacySubagentSourceRange {
    pub file: PathBuf,
    pub start: u64,
    pub end: u64,
}

pub(crate) async fn build_legacy_subagent_prompt(
    backend: &Arc<dyn SubagentBackend>,
    parent_session_id: &str,
    instructions: &str,
    context: Option<&str>,
    ranges: &[LegacySubagentSourceRange],
) -> Result<String, ApiFailure> {
    // Accepted wire requests retain their original validation and byte limit.
    let mut prompt = instructions.trim().to_owned();
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        prompt.push_str("\n\n<parent_context>\n");
        prompt.push_str(context);
        prompt.push_str("\n</parent_context>");
    }
    for range in ranges {
        if range.file.as_os_str().is_empty()
            || range.file.is_absolute()
            || range
                .file
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(ApiFailure::bad_request(format!(
                "source path {} must be relative and must not contain '..'",
                range.file.display()
            )));
        }
        if range.start == 0 || range.end < range.start {
            return Err(ApiFailure::bad_request(format!(
                "invalid source range {}:{}-{}; lines are one-based and inclusive",
                range.file.display(),
                range.start,
                range.end
            )));
        }
        let bytes = backend
            .read_context_file(parent_session_id.to_owned(), range.file.clone())
            .await
            .map_err(ApiFailure::from)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ApiFailure::bad_request(format!(
                "source file {} is not UTF-8 text",
                range.file.display()
            ))
        })?;
        let lines = text.lines().collect::<Vec<_>>();
        if range.end > lines.len() as u64 {
            return Err(ApiFailure::bad_request(format!(
                "source range {}:{}-{} exceeds its {} lines",
                range.file.display(),
                range.start,
                range.end,
                lines.len()
            )));
        }
        prompt.push_str(&format!(
            "\n\n--- source {:?}, lines {}-{} (one-based, inclusive) ---\n",
            range.file.to_string_lossy(),
            range.start,
            range.end
        ));
        for (offset, line) in lines[(range.start - 1) as usize..range.end as usize]
            .iter()
            .enumerate()
        {
            prompt.push_str(&format!("{:>6}  {line}\n", range.start as usize + offset));
        }
        prompt.push_str("--- end source ---");
        if prompt.len() > MAX_SUBAGENT_CONTEXT_BYTES {
            return Err(ApiFailure::bad_request(format!(
                "sub-agent handoff exceeds the {MAX_SUBAGENT_CONTEXT_BYTES}-byte limit"
            )));
        }
    }
    if prompt.len() > MAX_SUBAGENT_CONTEXT_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "sub-agent handoff exceeds the {MAX_SUBAGENT_CONTEXT_BYTES}-byte limit"
        )));
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_spawn_assignments_use_only_validated_instructions() {
        assert_eq!(
            build_subagent_prompt("  Read src/lib.rs:1-20 and report.\n").unwrap(),
            "Read src/lib.rs:1-20 and report."
        );
        for text in ["   ", " !shell"] {
            assert!(build_subagent_prompt(text).is_err());
        }
        assert!(build_subagent_prompt(&"🦀".repeat(MAX_SUBAGENT_CONTEXT_BYTES / 4)).is_ok());
        assert!(build_subagent_prompt(&"🦀".repeat(MAX_SUBAGENT_CONTEXT_BYTES / 4 + 1)).is_err());
        for (key, value) in [
            ("files", serde_json::json!([])),
            ("context", serde_json::json!("do not lose me")),
        ] {
            let mut request =
                serde_json::json!({"task_name":"task", "instructions":"read src/lib.rs"});
            request[key] = value;
            let error = serde_json::from_value::<SpawnSubagentRequest>(request).unwrap_err();
            assert!(error.to_string().contains("unknown field"));
            assert!(error.to_string().contains("instructions"));
        }
    }

    #[tokio::test]
    async fn accepted_legacy_spawn_preserves_context_and_source_ranges() {
        let action: mj_core::subagent::SubagentToolAction = serde_json::from_value(serde_json::json!({
            "action":"spawn", "params":{"task_name":"legacy", "instructions":"inspect", "context":"prior findings",
            "files":[{"file":"legacy.rs", "ranges":[{"start":2,"end":3}]}]}
        })).unwrap();
        let mj_core::subagent::SubagentToolAction::Spawn {
            instructions,
            context,
            files,
            ..
        } = action
        else {
            panic!("spawn expected")
        };
        let ranges = files
            .iter()
            .flat_map(|file| {
                file.ranges.iter().map(|range| LegacySubagentSourceRange {
                    file: file.file.clone(),
                    start: range.start,
                    end: range.end,
                })
            })
            .collect::<Vec<_>>();
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![],
                unavailable: vec![],
            },
        });
        let prompt = build_legacy_subagent_prompt(
            &backend,
            "parent",
            &instructions,
            context.as_deref(),
            &ranges,
        )
        .await
        .unwrap();
        assert!(prompt.starts_with("inspect\n\n<parent_context>\nprior findings"));
        assert!(
            prompt.contains("     2  selected\n     3  evidence"),
            "{prompt}"
        );
        assert!(!prompt.contains("unselected"));
        let long_instructions = "x".repeat(65_537);
        assert_eq!(
            build_legacy_subagent_prompt(&backend, "parent", &long_instructions, None, &[])
                .await
                .unwrap(),
            long_instructions
        );
    }

    fn candidate(profile_id: &str, remaining: Option<u8>, models: &[&str]) -> SubagentCandidate {
        SubagentCandidate {
            profile_id: profile_id.to_owned(),
            harness: mj_core::config::HarnessKind::Codex,
            choices: mj_core::worker_launch::ProfileConfig {
                model: models.first().map(|model| (*model).to_owned()),
                models: models
                    .iter()
                    .map(|model| mj_core::acp::SessionConfigChoice {
                        value: (*model).to_owned(),
                        name: (*model).to_owned(),
                        description: None,
                    })
                    .collect(),
                efforts: Vec::new(),
                observed_at: 1,
            },
            quota: remaining.map_or(CandidateQuota::Unknown, |remaining| {
                CandidateQuota::Subscription {
                    monthly_price_usd: None,
                    refreshed_at_epoch_seconds: 0,
                    paid_credits: None,
                    windows: vec![crate::quota::QuotaWindow {
                        label: "Week".into(),
                        remaining_percent: Some(remaining),
                        duration_seconds: None,
                        model_scope: None,
                        used: None,
                        limit: None,
                        resets: None,
                        resets_at_epoch_seconds: None,
                    }],
                }
            }),
            unhealthy: Default::default(),
        }
    }

    fn ids(candidates: &[SubagentCandidate]) -> Vec<&str> {
        candidates
            .iter()
            .map(|candidate| candidate.profile_id.as_str())
            .collect()
    }

    // Hard-won: 435b4ed9: children used the parent's nearly depleted profile over one with quota
    #[test]
    fn ranking_puts_most_quota_first_then_unknown_api_and_empty_and_ties_to_the_parent() {
        let mut candidates = vec![
            SubagentCandidate {
                quota: CandidateQuota::UsagePriced,
                ..candidate("api", None, &["luna"])
            },
            candidate("unknown", None, &["luna"]),
            candidate("b-other", Some(40), &["luna"]),
            candidate("parent", Some(40), &["luna"]),
            candidate("a-other", Some(40), &["luna"]),
            candidate("high", Some(90), &["luna"]),
            candidate("empty", Some(0), &["luna"]),
        ];
        rank_candidates(&mut candidates, Some("parent"), &[], "luna", 0);
        assert_eq!(
            ids(&candidates),
            vec![
                "high", "parent", "a-other", "b-other", "unknown", "api", "empty"
            ]
        );
    }

    #[test]
    fn load_balancing_preferences_lead_among_quota_equals_in_list_order() {
        let mut candidates = vec![
            candidate("a-plain", Some(50), &["luna"]),
            candidate("b-preferred", Some(50), &["luna"]),
            candidate("c-preferred-first", Some(50), &["luna"]),
        ];
        rank_candidates(
            &mut candidates,
            None,
            &["c-preferred-first".into(), "b-preferred".into()],
            "luna",
            0,
        );
        assert_eq!(ids(&candidates), ["c-preferred-first", "b-preferred", "a-plain"]);
        // An exhausted preference does not win: quota still owns feasibility.
        let mut candidates = vec![
            candidate("plain", Some(50), &["luna"]),
            candidate("preferred-out", Some(0), &["luna"]),
        ];
        rank_candidates(&mut candidates, None, &["preferred-out".into()], "luna", 0);
        assert_eq!(ids(&candidates), ["plain", "preferred-out"]);
    }

    #[test]
    fn scoped_exhaustion_ranks_only_the_scoped_model_last() {
        let mut fable = candidate(
            "fable-profile",
            Some(80),
            &["claude-fable-5-1", "claude-opus-5"],
        );
        let CandidateQuota::Subscription {
            windows,
            paid_credits,
            ..
        } = &mut fable.quota
        else {
            unreachable!("the fixture has a reading");
        };
        *paid_credits = mj_client::quota::PaidCredits::limited(1e12);
        windows.push(crate::quota::QuotaWindow {
            label: "Week (Fable)".into(),
            remaining_percent: Some(0),
            duration_seconds: Some(604_800),
            model_scope: Some("fable".into()),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        });
        let other = candidate("other", Some(40), &["claude-fable-5-1", "claude-opus-5"]);

        let mut for_fable = vec![fable.clone(), other.clone()];
        rank_candidates(&mut for_fable, None, &[], "claude-fable-5-1", 1_000);
        assert_eq!(ids(&for_fable), ["other", "fable-profile"]);

        let mut for_opus = vec![fable, other];
        rank_candidates(&mut for_opus, None, &[], "claude-opus-5", 1_000);
        assert_eq!(ids(&for_opus), ["fable-profile", "other"]);
    }

    #[test]
    fn five_hour_burn_projected_to_run_out_sorts_last_within_its_tier() {
        let now = 1_000_000;
        let with_five_hour = |id: &str, remaining: u8| {
            let mut profile = candidate(id, Some(90), &["luna"]);
            let CandidateQuota::Subscription {
                windows,
                refreshed_at_epoch_seconds,
                ..
            } = &mut profile.quota
            else {
                unreachable!("the fixture has a reading");
            };
            // Read one hour into the window, so using more than 20% outpaces it.
            *refreshed_at_epoch_seconds = now as u64;
            windows.push(crate::quota::QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(remaining),
                duration_seconds: None,
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(now + 4 * 60 * 60),
            });
            profile
        };
        let profiles = vec![
            candidate("unknown", None, &["luna"]),
            // Scores 1.4, ahead of "steady" on pace alone.
            with_five_hour("burning", 70),
            candidate("steady", Some(40), &["luna"]),
            with_five_hour("pacing", 85),
        ];

        let mut ranked = profiles.clone();
        rank_candidates(&mut ranked, None, &[], "luna", now);
        assert_eq!(ids(&ranked), ["pacing", "steady", "burning", "unknown"]);

        for unit in [1e-9, 0.5, 1.0] {
            let mut drawn = profiles.clone();
            let units = vec![unit; drawn.len()];
            order_by_draw(&mut drawn, None, &[], &units, "luna", now);
            assert_eq!(ids(&drawn)[2..], ["burning", "unknown"]);
        }

        // The pace was measured at the reading: an unchanged reading still
        // projects later, until the window resets.
        let mut stale = profiles.clone();
        rank_candidates(&mut stale, None, &[], "luna", now + 30 * 60);
        assert_eq!(ids(&stale), ["pacing", "steady", "burning", "unknown"]);
        let mut reset = profiles.clone();
        rank_candidates(&mut reset, None, &[], "luna", now + 4 * 60 * 60);
        assert_eq!(ids(&reset), ["burning", "pacing", "steady", "unknown"]);
    }

    #[test]
    fn drawing_picks_in_proportion_to_subscription_dollars_and_quota_then_paid_balances() {
        // A fixed stream proves the distribution and prevents one large paid
        // wallet from jumping ahead of included subscription usage.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut unit = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) + 1) as f64 / (1u64 << 53) as f64
        };
        for (included, price_weighted) in [(true, false), (true, true), (false, true)] {
            let mut profiles = vec![
                SubagentCandidate {
                    quota: CandidateQuota::UsagePriced,
                    ..candidate("api", None, &["luna"])
                },
                candidate("unknown", None, &["luna"]),
                candidate("empty", Some(0), &["luna"]),
            ];
            for (id, amount) in [("sixty", 60), ("thirty-a", 30), ("thirty-b", 30)] {
                let mut profile = candidate(
                    id,
                    Some(if !included {
                        0
                    } else if price_weighted {
                        90
                    } else {
                        amount
                    }),
                    &["luna"],
                );
                let CandidateQuota::Subscription {
                    paid_credits,
                    monthly_price_usd,
                    ..
                } = &mut profile.quota
                else {
                    unreachable!()
                };
                if price_weighted {
                    *monthly_price_usd = Some(if amount == 60 { 200 } else { 100 });
                }
                // Opposite paid weights while quota remains must have no effect.
                *paid_credits = mj_client::quota::PaidCredits::limited(if included {
                    if amount == 60 { 0.01 } else { 1e12 }
                } else {
                    f64::from(amount)
                });
                profiles.push(profile);
            }
            let draws = 40_000;
            let mut first = std::collections::BTreeMap::<String, u32>::new();
            for _ in 0..draws {
                let mut candidates = profiles.clone();
                let units = (0..candidates.len()).map(|_| unit()).collect::<Vec<_>>();
                order_by_draw(&mut candidates, Some("thirty-b"), &[], &units, "luna", 0);
                let chosen = if included {
                    assert_eq!(ids(&candidates)[3..], ["unknown", "api", "empty"]);
                    &candidates[0]
                } else {
                    assert_eq!(candidates[0].profile_id, "unknown");
                    assert_eq!(ids(&candidates)[4..], ["api", "empty"]);
                    &candidates[1]
                };
                *first.entry(chosen.profile_id.clone()).or_default() += 1;
            }
            let share = |id: &str| f64::from(first[id]) / f64::from(draws);
            assert!((share("sixty") - 0.5).abs() < 0.01, "{first:?}");
            assert!((share("thirty-a") - 0.25).abs() < 0.01, "{first:?}");
            assert!((share("thirty-b") - 0.25).abs() < 0.01, "{first:?}");
        }
    }

    #[tokio::test]
    async fn a_pinned_profile_must_be_eligible_and_offer_the_model() {
        let candidates = || SubagentCandidates {
            offered: vec![candidate("codex", Some(5), &["luna"])],
            unavailable: vec![("glm".to_owned(), "not signed in".to_owned())],
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: candidates(),
        });
        let choose = |profile: &'static str, model: &'static str| {
            let backend = Arc::clone(&backend);
            let candidates = candidates();
            async move {
                resolve_model_profile_selection_from(
                    &backend,
                    candidates,
                    Some(profile),
                    model,
                    EffortRequirement::Any,
                    None,
                    ModelProfileCandidateSource::Subagents {
                        parent_profile: "codex",
                    },
                )
                .await
            }
        };
        assert_eq!(choose("codex", "luna").await.unwrap().profile_id, "codex");
        let not_offered = choose("codex", "sol").await.unwrap_err();
        assert!(
            not_offered.message.contains("does not offer \"sol\""),
            "{}",
            not_offered.message
        );
        let unavailable = choose("kimi", "luna").await.unwrap_err();
        assert!(
            unavailable.message.contains("unavailable"),
            "{}",
            unavailable.message
        );
        let broken = choose("glm", "luna").await.unwrap_err();
        assert!(
            broken.message.contains("not signed in"),
            "{}",
            broken.message
        );
    }

    // Hard-won: 435b4ed9: same-harness profiles hid models needed by sub-agents
    #[test]
    fn merging_keeps_the_best_of_each_same_model_group() {
        let merged = merge_same_models(
            vec![
                candidate("codex2", Some(3), &["nova", "luna"]),
                candidate("codex4", Some(60), &["luna", "nova"]),
                candidate("deepseek", Some(100), &["flash"]),
                candidate("codex3", Some(20), &["luna"]),
            ],
            "codex2",
            "nova",
            0,
        );
        assert_eq!(ids(&merged), vec!["deepseek", "codex4", "codex3"]);
    }

    /// Just enough of a backend to drive `resolve_subagent_selection`: one
    /// profile's candidates, and no live parent session (so effort inherits
    /// nothing and the parent's real model is never consulted).
    struct FakeSelectionBackend {
        ready_first: bool,
        /// Profiles that become ready one by one, each after the given
        /// seconds, for the cold-start fallback; none is ready before that.
        staged: Option<Vec<(u64, SubagentCandidates)>>,
        discovery_fails: bool,
        config: mj_core::config::SubagentConfig,
        candidates: SubagentCandidates,
    }

    impl SubagentBackend for FakeSelectionBackend {
        fn subagent_config(&self) -> BoxFuture<'_, AnyResult<mj_core::config::SubagentConfig>> {
            Box::pin(async { Ok(self.config.clone()) })
        }

        fn profile_config(
            &self,
            profile: String,
            model: Option<String>,
            _refresh: bool,
        ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
            Box::pin(async move {
                if profile.starts_with("stalled-") {
                    std::future::pending::<()>().await;
                }
                if profile.starts_with("delayed-") {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                if self.discovery_fails {
                    anyhow::bail!("model-specific probe failed: expired login");
                }
                let mut choices = self
                    .candidates
                    .offered
                    .iter()
                    .find(|candidate| candidate.profile_id == profile)
                    .unwrap()
                    .choices
                    .clone();
                anyhow::ensure!(
                    model.as_deref().is_none_or(|model| choices
                        .models
                        .iter()
                        .any(|choice| choice.value == model)),
                    "probe received an unadvertised model value"
                );
                // Like Claude Haiku: the profile's default model has efforts,
                // this model has none.
                if model.as_deref() == Some("no-effort") {
                    choices.efforts.clear();
                }
                // Only the model-specific discovery exposes high effort.
                if model.as_deref() == Some("fixed") && profile != "full-but-wrong-effort" {
                    choices.efforts = vec![mj_core::acp::SessionConfigChoice {
                        value: "high".into(),
                        name: "High".into(),
                        description: None,
                    }];
                }
                Ok(choices)
            })
        }
        fn subagent_candidates(
            &self,
            _parent_profile: String,
        ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
            let candidates = self.candidates.clone();
            Box::pin(async move { Ok(candidates) })
        }
        fn reviewer_candidates(&self) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
            if self.ready_first {
                return Box::pin(std::future::pending());
            }
            let candidates = self.candidates.clone();
            Box::pin(async move { Ok(candidates) })
        }
        fn reviewer_candidates_for_model(
            &self,
            _model: String,
            tried: Vec<String>,
        ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
            let Some(stages) = self.staged.clone() else {
                return self.reviewer_candidates();
            };
            Box::pin(async move {
                let mut elapsed = 0;
                for (at, candidates) in stages {
                    tokio::time::sleep(std::time::Duration::from_secs(at.saturating_sub(elapsed)))
                        .await;
                    elapsed = at;
                    if candidates
                        .offered
                        .iter()
                        .any(|candidate| !tried.contains(&candidate.profile_id))
                    {
                        return Ok(candidates);
                    }
                }
                anyhow::bail!("no further profile offers the model")
            })
        }
        fn reviewer_ready_candidates(
            &self,
            _model: String,
        ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
            if self.staged.is_some() {
                return Box::pin(async { Ok(SubagentCandidates::default()) });
            }
            let candidates = self.candidates.clone();
            Box::pin(async move { Ok(candidates) })
        }
        fn session_handle(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<Option<SessionHandle>>> {
            Box::pin(async { Ok(None) })
        }
        fn prompt(&self, _session_id: String, _text: String) -> BoxFuture<'_, AnyResult<u64>> {
            Box::pin(async { anyhow::bail!("not used in this test") })
        }
        fn turn_state(&self, _session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>> {
            Box::pin(async { Ok(None) })
        }
        fn turn_summary(
            &self,
            _session_id: String,
            _turn: TurnSpan,
        ) -> BoxFuture<'_, AnyResult<TurnSummary>> {
            Box::pin(async { anyhow::bail!("not used in this test") })
        }
        fn start_followup(
            &self,
            _session_id: String,
            _followup: StartFollowup,
        ) -> BoxFuture<'_, AnyResult<()>> {
            Box::pin(async { anyhow::bail!("not used in this test") })
        }
        fn start_status(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, AnyResult<Option<StartStatus>>> {
            Box::pin(async { Ok(None) })
        }
        fn transcript(
            &self,
            _session_id: String,
            _after_seq: u64,
            _limit: usize,
            _roles: Vec<mj_core::transcript::TranscriptRole>,
            _finished_only: bool,
        ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>> {
            Box::pin(async { Ok(None) })
        }
        fn diff(
            &self,
            _session_id: String,
            _options: DiffOptions,
        ) -> BoxFuture<'_, Result<String, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not used in this test".into())) })
        }
        fn read_context_file(
            &self,
            session: String,
            path: PathBuf,
        ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>> {
            Box::pin(async move {
                assert_eq!(session, "parent");
                assert_eq!(path, PathBuf::from("legacy.rs"));
                Ok(b"unselected\nselected\nevidence\nunselected\n".to_vec())
            })
        }
        fn read_file(
            &self,
            _session_id: String,
            _path: PathBuf,
            _mode: mj_checkpoint::archive::FetchMode,
        ) -> BoxFuture<'_, Result<mj_checkpoint::archive::FetchedPath, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not used in this test".into())) })
        }
        fn push_branch(
            &self,
            _session_id: String,
            _branch: String,
        ) -> BoxFuture<'_, Result<PushedBranch, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not used in this test".into())) })
        }
        fn bundle(&self, _session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>> {
            Box::pin(async { Err(ExportError::Refused("not used in this test".into())) })
        }
    }

    fn efforts(values: &[&str]) -> Vec<mj_core::acp::SessionConfigChoice> {
        values
            .iter()
            .map(|value| mj_core::acp::SessionConfigChoice {
                value: (*value).to_owned(),
                name: (*value).to_owned(),
                description: None,
            })
            .collect()
    }

    /// I1-2: a parent at `high` spawned Claude Haiku, which offers no efforts.
    /// The child inherited `high`, and every explicit effort the parent tried
    /// passed because it was checked against the profile's default model; the
    /// child's first prompt then never ran ("this agent does not offer high as
    /// a effort"). Efforts are the child model's own.
    // Hard-won: f67ed023: Haiku inherited an invalid effort from a sibling model and never ran its first prompt
    #[test]
    fn a_child_effort_comes_from_the_efforts_its_own_model_offers() {
        assert_eq!(
            child_effort("haiku", &[], None, Some("high")).unwrap(),
            None
        );
        assert_eq!(
            child_effort("sonnet", &efforts(&["low", "high"]), None, Some("high")).unwrap(),
            Some("high".into())
        );
        assert_eq!(
            child_effort("sonnet", &efforts(&["low"]), None, Some("high")).unwrap(),
            None
        );
        let none_offered = child_effort("haiku", &[], Some("low"), None).unwrap_err();
        assert!(
            none_offered.message.contains("offers no effort choices"),
            "{}",
            none_offered.message
        );
        let not_offered =
            child_effort("sonnet", &efforts(&["low"]), Some("high"), None).unwrap_err();
        assert!(
            not_offered.message.contains("choices: low"),
            "{}",
            not_offered.message
        );
    }
    // Hard-won: discovery failures with omitted effort panicked instead of refusing the spawn.
    #[tokio::test]
    async fn default_effort_spawn_reports_all_failed_model_discoveries() {
        use mj_core::config::{SubagentChoice, SubagentConfig};
        use mj_core::subagent::{SubagentPolicy, SubagentTaskType};
        let choice = SubagentChoice {
            model: "no-effort".into(),
            effort: None,
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: true,
            config: SubagentConfig {
                straightforward: Some(choice.clone()),
                subtle: Some(choice),
                ..Default::default()
            },
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("expired-first", Some(90), &["no-effort"]),
                    candidate("expired-second", Some(50), &["no-effort"]),
                ],
                unavailable: vec![],
            },
        });
        let error = resolve_subagent_policy_selection(
            &backend,
            "parent",
            &SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            },
            SubagentTaskType::Straightforward,
        )
        .await
        .unwrap_err();
        for detail in [
            "[subagents.straightforward]",
            "harness default effort",
            "no-effort",
            "expired-first",
            "expired-second",
            "expired login",
        ] {
            assert!(error.message.contains(detail), "{}", error.message);
        }
    }

    #[tokio::test]
    async fn reviewer_uses_ready_profile_without_waiting_for_pending_discovery() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: true,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![candidate("ready", Some(70), &["fixed"])],
                unavailable: Vec::new(),
            },
        });
        let selected = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            resolve_reviewer_model_profile_selection(&backend, "fixed", Some("high")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(selected.profile_id, "ready");
    }

    #[tokio::test]
    async fn legacy_pinned_reviewer_keeps_its_profile_when_another_offers_the_model() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("pinned", Some(0), &["fixed"]),
                    candidate("other", Some(90), &["fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_pinned_reviewer_model_profile_selection(
            &backend,
            "pinned",
            "fixed",
            Some("high"),
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "pinned");
    }

    #[tokio::test]
    async fn reviewer_draw_keeps_quota_ahead_of_cached_paid_efforts() {
        let mut paid = candidate("paid", Some(0), &["fixed"]);
        paid.choices.efforts = vec![mj_core::acp::SessionConfigChoice {
            value: "high".into(),
            name: "High".into(),
            description: None,
        }];
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![paid, candidate("subscription", Some(80), &["fixed"])],
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "subscription");
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_ready_later_candidate_bounds_earlier_probe_grace() {
        let mut ready = candidate("ready", Some(0), &["fixed"]);
        ready.choices.model = Some("fixed".into());
        ready.choices.efforts = vec![mj_core::acp::SessionConfigChoice {
            value: "high".into(),
            name: "High".into(),
            description: None,
        }];
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![candidate("stalled-first", Some(80), &["fixed"]), ready],
                unavailable: Vec::new(),
            },
        });
        let start = tokio::time::Instant::now();
        let selected = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "ready");
        assert_eq!(
            tokio::time::Instant::now().duration_since(start),
            std::time::Duration::from_secs(2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_completed_third_candidate_bounds_each_stalled_predecessor() {
        let mut ready = candidate("ready-third", Some(0), &["fixed"]);
        ready.choices.model = Some("fixed".into());
        ready.choices.efforts = vec![mj_core::acp::SessionConfigChoice {
            value: "high".into(),
            name: "High".into(),
            description: None,
        }];
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: (0..10)
                    .map(|index| candidate(&format!("stalled-{index}"), Some(80), &["fixed"]))
                    .chain(std::iter::once(ready))
                    .collect(),
                unavailable: Vec::new(),
            },
        });
        let start = tokio::time::Instant::now();
        let selected = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "ready-third");
        assert_eq!(
            tokio::time::Instant::now().duration_since(start),
            std::time::Duration::from_secs(2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_later_probe_completion_starts_grace_after_probe_finishes() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("stalled-first", Some(80), &["fixed"]),
                    candidate("delayed-later", Some(0), &["fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        let start = tokio::time::Instant::now();
        let selected = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "delayed-later");
        assert_eq!(
            tokio::time::Instant::now().duration_since(start),
            std::time::Duration::from_secs(3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_default_effort_skips_stalled_model_effort_probe() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![candidate("stalled-default", Some(80), &["fixed"])],
                unavailable: Vec::new(),
            },
        });
        let start = tokio::time::Instant::now();
        let selected = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Default,
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "stalled-default");
        assert_eq!(tokio::time::Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_cold_start_keeps_waiting_past_a_profile_that_cannot_serve_the_slot() {
        // Only the model-specific probe of "fixed" offers high; this profile
        // reports no effort for the model, so it cannot serve a high slot.
        let first = candidate("full-but-wrong-effort", Some(90), &["fixed"]);
        let second = candidate("second", Some(50), &["fixed"]);
        let staged = |ids: &[&SubagentCandidate]| SubagentCandidates {
            offered: ids.iter().map(|candidate| (*candidate).clone()).collect(),
            unavailable: Vec::new(),
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: Some(vec![
                (1, staged(&[&first])),
                (8, staged(&[&first, &second])),
            ]),
            discovery_fails: false,
            config: Default::default(),
            candidates: staged(&[&first, &second]),
        });
        let start = tokio::time::Instant::now();
        let selected = resolve_reviewer_model_profile_selection(&backend, "fixed", Some("high"))
            .await
            .unwrap();
        assert_eq!(selected.profile_id, "second");
        let waited = tokio::time::Instant::now().duration_since(start);
        assert!(waited >= std::time::Duration::from_secs(8), "{waited:?}");
        assert!(waited < std::time::Duration::from_secs(20), "{waited:?}");

        // When no later profile can serve it, the deadline still ends the wait.
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: Some(vec![
                (1, staged(&[&first])),
                (30, staged(&[&first, &second])),
            ]),
            discovery_fails: false,
            config: Default::default(),
            candidates: staged(&[&first, &second]),
        });
        let start = tokio::time::Instant::now();
        let error = resolve_reviewer_model_profile_selection(&backend, "fixed", Some("high"))
            .await
            .unwrap_err();
        assert!(
            tokio::time::Instant::now().duration_since(start) <= std::time::Duration::from_secs(20)
        );
        assert!(error.message.contains("timed out"), "{}", error.message);
    }

    #[tokio::test(start_paused = true)]
    async fn reviewer_stalled_probes_share_one_deadline() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("stalled-one", Some(80), &["fixed"]),
                    candidate("stalled-two", Some(70), &["fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        let start = tokio::time::Instant::now();
        let result = resolve_model_profile_selection_matching(
            &backend,
            None,
            "fixed",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::ReviewersReady,
        )
        .await;
        assert!(result.is_err());
        assert!(
            tokio::time::Instant::now().duration_since(start) <= std::time::Duration::from_secs(20)
        );
    }

    #[tokio::test]
    async fn reviewer_selection_shares_spawn_effort_health_and_draw_rules() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("first", Some(70), &["no-effort", "fixed"]),
                    candidate("second", Some(70), &["no-effort", "fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_reviewer_model_profile_selection(&backend, "fixed", Some("high"))
            .await
            .unwrap();
        assert_eq!(selected.effort.as_deref(), Some("high"));
        let error = resolve_reviewer_model_profile_selection(&backend, "missing", None)
            .await
            .unwrap_err();
        assert!(
            error.message.contains("no enabled profile"),
            "{}",
            error.message
        );
        let mut saw_different_profiles = false;
        for _ in 0..48 {
            let first = resolve_reviewer_model_profile_selection(&backend, "no-effort", None)
                .await
                .unwrap();
            let second = resolve_reviewer_model_profile_selection(&backend, "no-effort", None)
                .await
                .unwrap();
            assert_eq!(first.effort, None);
            assert_eq!(second.effort, None);
            saw_different_profiles |= first.profile_id != second.profile_id;
        }
        assert!(
            saw_different_profiles,
            "same-model reviewer slots never drew different profiles"
        );
    }

    // Hard-won: #1534: exact alias matching stranded Antigravity's Claude quota.
    #[tokio::test]
    async fn sonnet_alias_balances_scoped_quota_and_launches_each_harness_value() {
        use mj_core::config::{HarnessKind, SubagentChoice, SubagentConfig};
        use mj_core::subagent::{SubagentPolicy, SubagentTaskType};
        let recorded: std::collections::BTreeMap<String, Vec<mj_core::acp::SessionConfigChoice>> =
            serde_json::from_str(include_str!(
                "../../../../mj-core/tests/fixtures/claude-antigravity-models.json"
            ))
            .unwrap();
        for (claude_left, gemini_left, expected, value) in [
            (0, 100, "gemini", "claude-sonnet-5-5"),
            (100, 0, "claude", "sonnet"),
        ] {
            let mut claude = candidate("claude", Some(claude_left), &["sonnet"]);
            claude.harness = HarnessKind::Claude;
            claude.choices.models = recorded["claude"].clone();
            claude.choices.model = Some("opus".into());
            let mut gemini = candidate("gemini", Some(gemini_left), &["claude-sonnet-5-5"]);
            gemini.harness = HarnessKind::Antigravity;
            gemini.choices.models = recorded["antigravity"].clone();
            gemini.choices.model = Some("gemini-4-carbon".into());
            if let CandidateQuota::Subscription { windows, .. } = &mut gemini.quota {
                windows[0].model_scope = Some("claude,gpt".into());
                let mut unrelated = windows[0].clone();
                unrelated.model_scope = Some("gemini".into());
                unrelated.remaining_percent = Some(0);
                windows.push(unrelated);
            }
            for c in [&mut claude, &mut gemini] {
                c.choices.efforts = vec![mj_core::acp::SessionConfigChoice {
                    value: "high".into(),
                    name: "High".into(),
                    description: None,
                }];
            }
            // An always-full account running an older Sonnet must never join this draw.
            let mut stale = candidate("stale", Some(100), &["sonnet"]);
            stale.choices.models[0].name = "Sonnet 5".into();
            let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
                ready_first: false,
                staged: None,
                discovery_fails: false,
                config: SubagentConfig {
                    straightforward: Some(SubagentChoice {
                        model: "sonnet".into(),
                        effort: Some("high".into()),
                    }),
                    ..Default::default()
                },
                candidates: SubagentCandidates {
                    offered: vec![stale, gemini, claude],
                    unavailable: vec![],
                },
            });
            for request in ["sonnet", "claude-sonnet-5-5"] {
                let selected = resolve_oracle_selection(
                    &backend,
                    "parent",
                    &SubagentChoice {
                        model: request.into(),
                        effort: Some("high".into()),
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    (
                        selected.profile_id.as_str(),
                        selected.model.as_str(),
                        selected.effort.as_deref()
                    ),
                    (expected, value, Some("high"))
                );
                let reviewer =
                    resolve_reviewer_model_profile_selection(&backend, request, Some("high"))
                        .await
                        .unwrap();
                assert_eq!(reviewer, selected);
            }
            let selected = resolve_subagent_policy_selection(
                &backend,
                "parent",
                &SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                },
                SubagentTaskType::Straightforward,
            )
            .await
            .unwrap();
            assert_eq!(
                (selected.profile_id.as_str(), selected.model.as_str()),
                (expected, value)
            );
        }
    }

    #[tokio::test]
    async fn task_type_spawn_uses_global_choices_and_quota_ranked_enabled_profiles() {
        use mj_core::config::{SubagentChoice, SubagentConfig};
        use mj_core::subagent::{SubagentPolicy, SubagentTaskType};
        let candidates = SubagentCandidates {
            offered: vec![
                // No quota left, so the weighted draw never puts it first.
                candidate("parent", Some(0), &["fixed", "no-effort"]),
                candidate("best", Some(70), &["fixed", "no-effort"]),
                candidate("full-but-wrong-effort", Some(100), &["fixed"]),
            ],
            unavailable: vec![],
        };
        let config = SubagentConfig {
            straightforward: Some(SubagentChoice {
                model: "no-effort".into(),
                effort: None,
            }),
            subtle: Some(SubagentChoice {
                model: "fixed".into(),
                effort: Some("high".into()),
            }),
            ..Default::default()
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: config.clone(),
            candidates: candidates.clone(),
        });
        for (task_type, model, effort) in [
            (SubagentTaskType::Straightforward, "no-effort", None),
            (SubagentTaskType::Subtle, "fixed", Some("high")),
        ] {
            let selected = resolve_subagent_policy_selection(
                &backend,
                "parent",
                &SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                },
                task_type,
            )
            .await
            .unwrap();
            assert_eq!(
                (
                    selected.profile_id.as_str(),
                    selected.model.as_str(),
                    selected.effort.as_deref()
                ),
                ("best", model, effort)
            );
        }

        // A profile whose sessions just failed at the provider is skipped for
        // that model despite its quota; with none left, the refusal says until
        // when (#1361).
        let marked = |profiles: &[&str]| -> Arc<dyn SubagentBackend> {
            let mut candidates = candidates.clone();
            for candidate in &mut candidates.offered {
                if profiles.contains(&candidate.profile_id.as_str()) {
                    candidate.unhealthy.insert(
                        "fixed".into(),
                        crate::profile_health::Unhealthy {
                            fault: crate::profile_health::ProfileFault::RateLimited,
                            until: std::time::SystemTime::UNIX_EPOCH
                                + std::time::Duration::from_secs(1_792_937_100),
                            message: "exceeded retry limit, last status: 429".into(),
                        },
                    );
                }
            }
            Arc::new(FakeSelectionBackend {
                ready_first: false,
                staged: None,
                discovery_fails: false,
                config: config.clone(),
                candidates,
            })
        };
        let any_policy = SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        };
        let selected = resolve_subagent_policy_selection(
            &marked(&["best"]),
            "parent",
            &any_policy,
            SubagentTaskType::Subtle,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "parent");
        let refused = resolve_subagent_policy_selection(
            &marked(&["best", "parent"]),
            "parent",
            &any_policy,
            SubagentTaskType::Subtle,
        )
        .await
        .unwrap_err();
        assert!(
            refused.message.contains(
                "profile best is rate-limited until 14:05:00 UTC (exceeded retry limit, last status: 429)"
            ),
            "{}",
            refused.message
        );
        let unconfigured: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: Default::default(),
            candidates,
        });
        let mixed_policy = SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: Some(Box::new(SubagentChoice {
                model: "fixed".into(),
                effort: Some("high".into()),
            })),
        };
        for (task_type, model, effort) in [
            (SubagentTaskType::Straightforward, "no-effort", None),
            (SubagentTaskType::Subtle, "fixed", Some("high")),
        ] {
            let selected =
                resolve_subagent_policy_selection(&backend, "parent", &mixed_policy, task_type)
                    .await
                    .unwrap();
            assert_eq!(
                (
                    selected.profile_id.as_str(),
                    selected.model.as_str(),
                    selected.effort.as_deref()
                ),
                ("best", model, effort)
            );
        }
        let error = resolve_subagent_policy_selection(
            &unconfigured,
            "parent",
            &SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            },
            SubagentTaskType::Subtle,
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("[subagents.subtle]"));
        assert!(error.message.contains("--subagent-subtle"));

        // With only straightforward configured, a subtle request names the
        // type the parent can use.
        let straightforward_only: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            config: SubagentConfig {
                straightforward: Some(SubagentChoice {
                    model: "no-effort".into(),
                    effort: None,
                }),
                ..Default::default()
            },
            candidates: SubagentCandidates {
                offered: vec![candidate("best", Some(70), &["no-effort"])],
                unavailable: vec![],
            },
        });
        let policy = SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        };
        let error = resolve_subagent_policy_selection(
            &straightforward_only,
            "parent",
            &policy,
            SubagentTaskType::Subtle,
        )
        .await
        .unwrap_err();
        assert!(
            error.message.contains("task_type `straightforward`"),
            "{}",
            error.message
        );
        let selected = resolve_subagent_policy_selection(
            &straightforward_only,
            "parent",
            &policy,
            SubagentTaskType::Straightforward,
        )
        .await
        .unwrap();
        assert_eq!(selected.model, "no-effort");
    }
}
