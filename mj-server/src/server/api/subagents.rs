use mj_accounts::profile_scheduler::{Placement, ScheduledCandidate, guardian_eligible};
#[cfg(test)]
use mj_accounts::profile_scheduler::{
    SchedulingContext, order_scheduled_candidates_by_draw, rank_scheduled_candidates,
};
use mj_core::config::ExecutionPolicy;
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
    let execution_policy = if backend
        .session_is_yolo_target(parent_session_id.clone())
        .await?
    {
        ExecutionPolicy::Unconstrained
    } else {
        ExecutionPolicy::ConfiguredApprovals
    };
    let task_type = request.task_type.unwrap_or_default();
    let selection = resolve_subagent_policy_selection(
        &backend,
        &parent.profile_id,
        &parent.subagents,
        task_type,
        execution_policy,
    )
    .await?;

    let relation = backend
        .start_subagent(mj_session_types::RegisterSubagentRequest {
            no_github_credentials: request.no_github_credentials,
            kind: mj_core::subagent::SubagentKind::Worker,
            orchestrator: task_type == mj_core::subagent::SubagentTaskType::Orchestrator,
            reviewer_launch: None,
            parent_session_id: parent_session_id.clone(),
            task_name: request.task_name,
            profile_id: selection.profile_id.clone(),
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
            category: task_type.id().to_owned(),
        })
        .await
        .map_err(|error| ApiFailure::conflict(format!("sub-agent creation failed: {error:#}")))?;
    selection
        .reservation
        .commit(Some(relation.child_session_id.clone()));
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

/// Shared RAII handle for an in-flight quota placement reservation.
#[derive(Debug, Default, Clone)]
pub struct ReservationSlot(
    std::sync::Arc<std::sync::Mutex<Option<mj_accounts::quota_telemetry::PlacementReservation>>>,
);

impl PartialEq for ReservationSlot {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ReservationSlot {}

impl ReservationSlot {
    pub fn new(reservation: Option<mj_accounts::quota_telemetry::PlacementReservation>) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(reservation)))
    }

    pub fn commit(&self, session_id: Option<String>) {
        if let Some(reservation) = self.0.lock().unwrap_or_else(|p| p.into_inner()).take() {
            reservation.commit(session_id);
        }
    }
}

/// The profile, model and effort a spawn runs its child with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSelection {
    pub profile_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub fast_mode: bool,
    pub reservation: ReservationSlot,
}

/// Profile chosen by load-balancing across a single quota provider.
pub struct ProviderSessionSelection {
    pub profile_id: String,
    pub reservation: ReservationSlot,
}

#[derive(Debug, Clone)]
struct ModelCandidate {
    candidate: SubagentCandidate,
    model: model_catalog::ModelOffer,
    scheduled: ScheduledCandidate,
}

impl AsRef<SubagentCandidate> for ModelCandidate {
    fn as_ref(&self) -> &SubagentCandidate {
        &self.candidate
    }
}

impl AsRef<ScheduledCandidate> for ModelCandidate {
    fn as_ref(&self) -> &ScheduledCandidate {
        &self.scheduled
    }
}

impl std::ops::Deref for ModelCandidate {
    type Target = SubagentCandidate;
    fn deref(&self) -> &Self::Target {
        &self.candidate
    }
}

#[derive(Debug, Clone)]
struct ChoiceCandidate {
    candidate: SubagentCandidate,
    model: model_catalog::ModelOffer,
    choice_effort: Option<String>,
    scheduled: ScheduledCandidate,
}

impl AsRef<SubagentCandidate> for ChoiceCandidate {
    fn as_ref(&self) -> &SubagentCandidate {
        &self.candidate
    }
}

impl AsRef<ScheduledCandidate> for ChoiceCandidate {
    fn as_ref(&self) -> &ScheduledCandidate {
        &self.scheduled
    }
}

fn scheduled_candidate(
    candidate: &SubagentCandidate,
    model: &str,
    effort: Option<String>,
) -> ScheduledCandidate {
    ScheduledCandidate {
        profile_id: candidate.profile_id.clone(),
        harness: candidate.harness,
        model: model.to_owned(),
        effort,
        quota: candidate.quota.clone(),
        may_consume_credits: candidate.may_consume_credits,
        cache_identity: candidate.cache_identity.clone(),
    }
}

/// Resolve the session override or global task choices, drawing a profile by quota.
pub async fn resolve_subagent_policy_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    policy: &mj_core::subagent::SubagentPolicy,
    task_type: mj_core::subagent::SubagentTaskType,
    execution_policy: ExecutionPolicy,
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
    let choices = config.effective_choices(policy, task_type);
    if choices.is_empty() {
        // A parent launched while subtle was configured can still ask for it.
        if task_type == mj_core::subagent::SubagentTaskType::Subtle
            && !config
                .effective_choices(policy, mj_core::subagent::SubagentTaskType::Straightforward)
                .is_empty()
        {
            return Err(ApiFailure::conflict(
                "The user configured only the straightforward task type; spawn this child with task_type `straightforward`",
            ));
        }
        return Err(ApiFailure::conflict(format!(
            "Configure [subagents.{}] or pass --subagent-{} <MODEL> before spawning this Mjolnir sub-agent",
            task_type.id(),
            task_type.id()
        )));
    }
    resolve_choices_selection(
        backend,
        choices,
        ModelProfileCandidateSource::Subagents { parent_profile },
        execution_policy,
        Some(task_type.id()),
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

/// The profile and effort for the oracle's model choices. Like a spawn, it
/// ranks the enabled profiles that offer any configured model by quota.
pub async fn resolve_oracle_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    choices: &[mj_core::config::SubagentChoice],
    execution_policy: ExecutionPolicy,
) -> Result<SubagentSelection, ApiFailure> {
    resolve_choices_selection(
        backend,
        choices,
        ModelProfileCandidateSource::Subagents { parent_profile },
        execution_policy,
        Some("oracle"),
    )
    .await
}

/// Load-balance a bare `mj new` session across configured `[[subagents.primary]]`
/// choices.
pub async fn resolve_primary_session_selection(
    backend: &Arc<dyn SubagentBackend>,
    ranking_anchor: Option<&str>,
    choices: &[mj_core::config::SubagentChoice],
    execution_policy: ExecutionPolicy,
) -> Result<SubagentSelection, ApiFailure> {
    resolve_choices_selection(
        backend,
        choices,
        ModelProfileCandidateSource::Session { ranking_anchor },
        execution_policy,
        Some("primary"),
    )
    .await
}

/// Load-balance across enabled profiles belonging to `provider` using the
/// shared quota-weighted profile scheduler.
pub async fn resolve_provider_session_selection(
    backend: &Arc<dyn SubagentBackend>,
    config: &mj_core::config::Config,
    provider: &mj_core::config::QuotaProvider,
    execution_policy: ExecutionPolicy,
    subagents: Option<&mj_core::subagent::SubagentPolicy>,
) -> Result<ProviderSessionSelection, ApiFailure> {
    let matching_ids: std::collections::BTreeSet<String> = {
        let profiles: Vec<(String, mj_core::config::HarnessProfile)> = config
            .enabled_profiles()
            .map(|(id, profile)| (id.to_owned(), profile.clone()))
            .collect();
        let target = provider.clone();
        mj_core::runtime::spawn_blocking(move || {
            profiles
                .into_iter()
                .filter(|(_, profile)| profile.quota_provider() == target)
                .map(|(id, _)| id)
                .collect()
        })
        .await
        .map_err(|error| ApiFailure::from(anyhow::anyhow!("resolve provider profiles: {error}")))?
    };
    if matching_ids.is_empty() {
        return Err(ApiFailure::bad_request(format!(
            "no enabled profile belongs to provider {}",
            provider.display_name()
        )));
    }
    let SubagentCandidates {
        offered,
        unavailable,
    } = backend.session_profile_candidates().await?;
    let now = now_epoch_seconds();
    let mut matched_any = false;
    let mut excluded = Vec::new();
    let mut eligible = Vec::new();
    for candidate in &offered {
        if !matching_ids.contains(&candidate.profile_id) {
            continue;
        }
        matched_any = true;
        if !guardian_eligible(candidate.harness, execution_policy) {
            excluded.push(format!(
                "profile {} ({}) does not support native guardian approvals on this target",
                candidate.profile_id,
                candidate.harness.display_name()
            ));
            continue;
        }
        if let Some(mj_core::subagent::SubagentPolicy::Mjolnir { .. }) = subagents
            && !candidate.harness.supports_delegation_tools()
        {
            excluded.push(format!(
                "profile {} ({}) does not support Mjolnir sub-agent delegation",
                candidate.profile_id,
                candidate.harness.display_name()
            ));
            continue;
        }
        let current_model = candidate
            .choices
            .model
            .as_deref()
            .or_else(|| {
                candidate
                    .choices
                    .models
                    .first()
                    .map(|choice| choice.value.as_str())
            })
            .unwrap_or("");
        if let Some(reason) =
            mj_accounts::profile_scheduler::ineligibility(candidate, current_model, now)
        {
            excluded.push(reason.describe(&candidate.profile_id, current_model));
            continue;
        }
        let scheduled = scheduled_candidate(candidate, current_model, None);
        eligible.push((candidate.clone(), current_model.to_owned(), scheduled));
    }
    if !matched_any {
        let reasons: Vec<String> = unavailable
            .into_iter()
            .filter(|(id, _)| matching_ids.contains(id))
            .map(|(id, reason)| format!("{id}: {reason}"))
            .collect();
        let detail = if reasons.is_empty() {
            String::new()
        } else {
            format!(" ({})", reasons.join("; "))
        };
        return Err(ApiFailure::conflict(format!(
            "no profile in provider {} is currently available{detail}",
            provider.display_name()
        )));
    }
    if eligible.is_empty() {
        return Err(ApiFailure::conflict(format!(
            "every {} profile is ineligible: {}",
            provider.display_name(),
            excluded.join("; ")
        )));
    }
    let mut unhealthy = Vec::new();
    eligible.retain(|(candidate, model, _)| {
        if let Some(note) = unhealthy_note(candidate, model) {
            unhealthy.push(note);
            false
        } else {
            true
        }
    });
    if eligible.is_empty() {
        return Err(ApiFailure::conflict(format!(
            "every eligible {} profile is marked unhealthy: {}",
            provider.display_name(),
            unhealthy.join("; ")
        )));
    }
    // When Native delegation is requested on a mixed-harness provider, prefer
    // profiles with native sub-agents; if none in the provider have them (e.g.
    // an Antigravity-only or DSH-only provider), keep them so the launch can
    // fall back to None rather than failing.
    if matches!(subagents, Some(mj_core::subagent::SubagentPolicy::Native))
        && eligible
            .iter()
            .any(|(candidate, _, _)| candidate.harness.has_native_subagents())
    {
        eligible.retain(|(candidate, _, _)| candidate.harness.has_native_subagents());
    }
    let prefer = backend.quota_preferences().await?;
    let power_ranking = backend.power_ranking().await?;
    let telemetry_store = backend.quota_telemetry_store();
    let mut draw_pool: Vec<ScheduledCandidate> = eligible
        .into_iter()
        .map(|(_, _, scheduled)| scheduled)
        .collect();
    let reservation = ReservationSlot::new(telemetry_store.draw_and_reserve(
        &mut draw_pool,
        execution_policy,
        Placement::TopLevel,
        None,
        None,
        &prefer,
        &power_ranking,
        now_epoch_seconds(),
    )?);
    let Some(chosen) = draw_pool.into_iter().next() else {
        return Err(ApiFailure::conflict(format!(
            "no eligible {} profile has available quota",
            provider.display_name()
        )));
    };
    Ok(ProviderSessionSelection {
        profile_id: chosen.profile_id,
        reservation,
    })
}

async fn resolve_choices_selection(
    backend: &Arc<dyn SubagentBackend>,
    choices: &[mj_core::config::SubagentChoice],
    source: ModelProfileCandidateSource<'_>,
    execution_policy: ExecutionPolicy,
    category: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    for choice in choices {
        if choice.effort.as_deref() == Some("adaptive") {
            return Err(ApiFailure::bad_request(
                "adaptive sub-agent effort is not supported",
            ));
        }
    }
    if let [single] = choices {
        return resolve_model_profile_selection_matching(
            backend,
            None,
            &single.model,
            single
                .effort
                .as_deref()
                .map(EffortRequirement::Exact)
                .unwrap_or(EffortRequirement::Default),
            None,
            source,
            execution_policy,
            category,
        )
        .await;
    }
    if choices.is_empty() {
        return Err(ApiFailure::bad_request("no model choices configured"));
    }
    let candidates = match source {
        ModelProfileCandidateSource::Session { .. } => backend.session_profile_candidates().await?,
        ModelProfileCandidateSource::Reviewers => backend.reviewer_candidates().await?,
        ModelProfileCandidateSource::ReviewersReady => {
            backend
                .reviewer_ready_candidates(choices[0].model.clone())
                .await?
        }
        ModelProfileCandidateSource::Subagents { parent_profile } => {
            backend
                .subagent_candidates(parent_profile.to_owned())
                .await?
        }
    };
    let SubagentCandidates {
        offered,
        unavailable,
    } = candidates;
    let all_models = offered
        .iter()
        .flat_map(|candidate| candidate.choices.models.iter().cloned())
        .collect::<Vec<_>>();
    let now = now_epoch_seconds();
    let mut matched_any = false;
    let mut excluded = Vec::new();
    let mut seen_pairs = std::collections::BTreeSet::new();
    let mut eligible = Vec::new();
    for choice in choices {
        let identity = model_catalog::model_id(&all_models, &choice.model);
        for candidate in &offered {
            let Some(offer) =
                model_catalog::offer(&candidate.choices.models, &identity, &choice.model)
            else {
                continue;
            };
            matched_any = true;
            if !guardian_eligible(candidate.harness, execution_policy) {
                excluded.push(format!(
                    "profile {} ({}) does not support native guardian approvals on this target",
                    candidate.profile_id,
                    candidate.harness.display_name()
                ));
                continue;
            }
            if let Some(reason) =
                mj_accounts::profile_scheduler::ineligibility(candidate, &identity, now)
            {
                excluded.push(reason.describe(&candidate.profile_id, &identity));
                continue;
            }
            let key = (
                candidate.profile_id.clone(),
                offer.value.clone(),
                choice.effort.clone(),
            );
            if seen_pairs.insert(key) {
                let scheduled = scheduled_candidate(candidate, &identity, choice.effort.clone());
                eligible.push(ChoiceCandidate {
                    candidate: candidate.clone(),
                    model: offer,
                    choice_effort: choice.effort.clone(),
                    scheduled,
                });
            }
        }
    }
    if !matched_any {
        let names = choices
            .iter()
            .map(|c| c.model.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiFailure::bad_request(no_profile_offers(
            &names,
            &offered,
            &unavailable,
            source.profile_description(),
        )));
    }
    if eligible.is_empty() {
        let names = choices
            .iter()
            .map(|c| c.model.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiFailure::conflict(format!(
            "every {} profile that can run configured models ({names}) is ineligible: {}.",
            source.profile_description(),
            excluded.join("; ")
        )));
    }
    let mut unhealthy = Vec::new();
    eligible.retain(|entry| {
        if let Some(note) = unhealthy_note(&entry.candidate, &entry.model.value) {
            unhealthy.push(note);
            false
        } else {
            true
        }
    });
    if eligible.is_empty() {
        return Err(ApiFailure::conflict(format!(
            "every eligible profile is marked unhealthy: {}",
            unhealthy.join("; ")
        )));
    }
    let prefer = backend.quota_preferences().await?;
    let power_ranking = backend.power_ranking().await?;
    let (anchor, placement) = match source {
        ModelProfileCandidateSource::Subagents { parent_profile } => (
            Some(parent_profile),
            if matches!(category, Some("supervisor" | "orchestrator")) {
                Placement::Orchestrator
            } else {
                Placement::Child
            },
        ),
        ModelProfileCandidateSource::Session { ranking_anchor } => {
            (ranking_anchor, Placement::TopLevel)
        }
        ModelProfileCandidateSource::Reviewers | ModelProfileCandidateSource::ReviewersReady => {
            (None, Placement::Child)
        }
    };
    let telemetry_store = backend.quota_telemetry_store();
    let mut selection_errors = Vec::new();
    while !eligible.is_empty() {
        let mut draw_pool = eligible.clone();
        let reservation = ReservationSlot::new(telemetry_store.draw_and_reserve(
            &mut draw_pool,
            execution_policy,
            placement,
            category,
            anchor,
            &prefer,
            &power_ranking,
            now_epoch_seconds(),
        )?);
        if draw_pool.is_empty() {
            break;
        }
        let entry = draw_pool.remove(0);
        eligible.retain(|item| {
            !(item.candidate.profile_id == entry.candidate.profile_id
                && item.model.value == entry.model.value
                && item.choice_effort == entry.choice_effort)
        });
        let efforts = match model_efforts(backend, &entry.candidate, &entry.model.value).await {
            Ok(efforts) => efforts,
            Err(error) => {
                selection_errors.push(format!("{}: {error:#}", entry.candidate.profile_id));
                continue;
            }
        };
        let effort = match entry.choice_effort.as_deref() {
            Some(requested) if efforts.iter().any(|c| c.value == requested) => {
                Some(requested.to_owned())
            }
            None => None,
            Some(_) => {
                selection_errors.push(format!(
                    "{} offers efforts: {}",
                    entry.candidate.profile_id,
                    effort_names(&efforts)
                ));
                continue;
            }
        };
        let mut selected = selection(
            entry.candidate.profile_id.clone(),
            &entry.model.value,
            effort,
        );
        selected.reservation = reservation;
        return Ok(selected);
    }
    if !unhealthy.is_empty() && selection_errors.is_empty() {
        return Err(ApiFailure::conflict(format!(
            "every eligible profile is marked unhealthy: {}",
            unhealthy.join("; ")
        )));
    }
    Err(ApiFailure::bad_request(format!(
        "no eligible profile supports the configured model/effort pairs ({})",
        selection_errors.join("; ")
    )))
}

/// Choose a profile for a user-created session from all enabled, usable
/// profiles and validate its effort.
pub(crate) async fn resolve_session_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    ranking_anchor: Option<&str>,
    model: &str,
    effort: Option<&str>,
    execution_policy: ExecutionPolicy,
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
        execution_policy,
        Some("primary"),
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
pub async fn resolve_reviewer_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    model: &str,
    effort: Option<&str>,
    execution_policy: ExecutionPolicy,
    category: &str,
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
            execution_policy,
            Some(category),
        )
        .await
        {
            return Ok(selected);
        }
        // A profile that offers the model but cannot serve this slot does not
        // end the search: later-discovered profiles may.
        let mut tried = Vec::new();
        let mut last_error: Option<ApiFailure> = None;
        // The profile IDs `last_error` already lists under "Could not check:".
        let mut last_error_listed: Vec<String> = Vec::new();
        // Profiles whose capability check failed, kept across iterations so a
        // later refusal for one profile never hides the others.
        let mut unavailable_profiles: Vec<(String, String)> = Vec::new();
        loop {
            let candidates = match backend
                .reviewer_candidates_for_model(model.to_owned(), tried.clone())
                .await
            {
                Ok(candidates) => candidates,
                Err(error) => {
                    return Err(with_could_not_check(
                        last_error.unwrap_or_else(|| error.into()),
                        &unavailable_profiles,
                        &last_error_listed,
                    ));
                }
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
            for (id, reason) in &candidates.unavailable {
                if !unavailable_profiles.iter().any(|(known, _)| known == id) {
                    unavailable_profiles.push((id.clone(), reason.clone()));
                }
            }
            if tried.len() == before {
                return Err(with_could_not_check(
                    last_error.unwrap_or_else(|| {
                        ApiFailure::unavailable(format!(
                            "no reviewer profile offers model {model:?}"
                        ))
                    }),
                    &unavailable_profiles,
                    &last_error_listed,
                ));
            }
            // Profiles that could only be reported as unavailable add no
            // selection result; keep the earlier refusal, which has the reason
            // a profile that did offer the model was refused.
            if candidates.offered.is_empty() && last_error.is_some() {
                continue;
            }
            let checked_ids = candidates
                .unavailable
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            match resolve_model_profile_selection_from(
                backend,
                candidates,
                None,
                model,
                requirement,
                None,
                ModelProfileCandidateSource::Reviewers,
                execution_policy,
                Some(category),
            )
            .await
            {
                Ok(selected) => return Ok(selected),
                Err(error) => {
                    last_error_listed = if error.message.contains("Could not check:") {
                        checked_ids
                    } else {
                        Vec::new()
                    };
                    last_error = Some(error);
                }
            }
        }
    })
    .await
    .map_err(|_| {
        ApiFailure::unavailable(format!(
            "reviewer model discovery timed out for model {model:?}"
        ))
    })?
}

/// Adds the profiles whose capabilities could not be checked to a refusal,
/// except the `already_listed` ones its message names.
fn with_could_not_check(
    mut failure: ApiFailure,
    unavailable: &[(String, String)],
    already_listed: &[String],
) -> ApiFailure {
    let missing = unavailable
        .iter()
        .filter(|(id, _)| !already_listed.contains(id))
        .map(|(id, reason)| format!("{id} ({reason})"))
        .collect::<Vec<_>>()
        .join("; ");
    if missing.is_empty() {
        return failure;
    }
    if failure.message.contains("Could not check:") {
        match failure.message.strip_suffix('.') {
            Some(listed) => failure.message = format!("{listed}; {missing}."),
            None => failure.message.push_str(&format!("; {missing}")),
        }
    } else if failure.message.ends_with('.') {
        failure
            .message
            .push_str(&format!(" Could not check: {missing}."));
    } else {
        failure
            .message
            .push_str(&format!(". Could not check: {missing}."));
    }
    failure
}

/// A pinned reviewer still runs only a model its profile offers, as the
/// profile's allow-list filters it. Its configured effort needs no discovery:
/// the harness reports a value it rejects.
pub async fn resolve_pinned_reviewer_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    profile: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let choices = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.profile_config(profile.to_owned(), None, false),
    )
    .await
    .map_err(|_| {
        ApiFailure::unavailable(format!(
            "review profile {profile:?} model discovery timed out"
        ))
    })?
    .map_err(|error| {
        ApiFailure::unavailable(format!(
            "review profile {profile:?} is unavailable: {error:#}"
        ))
    })?;
    validate_selectors(&choices, Some(model), None)?;
    Ok(selection(
        profile.to_owned(),
        model,
        effort.map(str::to_owned),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn resolve_model_profile_selection_matching(
    backend: &Arc<dyn SubagentBackend>,
    profile_id: Option<&str>,
    model: &str,
    requirement: EffortRequirement<'_>,
    inherited_effort: Option<&str>,
    source: ModelProfileCandidateSource<'_>,
    execution_policy: ExecutionPolicy,
    category: Option<&str>,
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
        execution_policy,
        category,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn resolve_model_profile_selection_from(
    backend: &Arc<dyn SubagentBackend>,
    candidates: SubagentCandidates,
    profile_id: Option<&str>,
    model: &str,
    requirement: EffortRequirement<'_>,
    inherited_effort: Option<&str>,
    source: ModelProfileCandidateSource<'_>,
    execution_policy: ExecutionPolicy,
    category: Option<&str>,
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
    let requested_effort = match requirement {
        EffortRequirement::Exact(e) => Some(e.to_owned()),
        EffortRequirement::Default | EffortRequirement::Any => inherited_effort.map(str::to_owned),
    };
    let mut offered = offered
        .into_iter()
        .filter_map(|candidate| {
            model_catalog::offer(&candidate.choices.models, &identity, model).map(|model| {
                let scheduled =
                    scheduled_candidate(&candidate, &identity, requested_effort.clone());
                ModelCandidate {
                    candidate,
                    model,
                    scheduled,
                }
            })
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
    let mut unhealthy = Vec::new();
    let mut draw_context = None;
    if profile_id.is_none() {
        // Load balancing offers only profiles that may still run the model
        // from their own allowance (or credits they may spend) and satisfy the
        // target's guardian policy. A named profile is not load balancing and
        // keeps its pin.
        let now = now_epoch_seconds();
        let mut excluded = Vec::new();
        let mut guardian_excluded = false;
        let mut quota_excluded = false;
        offered.retain(|candidate| {
            if !guardian_eligible(candidate.harness, execution_policy) {
                guardian_excluded = true;
                excluded.push(format!(
                    "profile {} ({}) does not support native guardian approvals on this target",
                    candidate.profile_id,
                    candidate.harness.display_name()
                ));
                return false;
            }
            match mj_accounts::profile_scheduler::ineligibility(
                &candidate.candidate,
                &identity,
                now,
            ) {
                Some(reason) => {
                    quota_excluded = true;
                    excluded.push(reason.describe(&candidate.profile_id, &identity));
                    false
                }
                None => true,
            }
        });
        if offered.is_empty() {
            let headline = if quota_excluded && !guardian_excluded {
                format!(
                    "every {} profile that can run model {model:?} is out of subscription quota: {}. \
                     Try again after the quota resets.",
                    source.profile_description(),
                    excluded.join("; ")
                )
            } else {
                format!(
                    "every {} profile that can run model {model:?} is ineligible: {}.",
                    source.profile_description(),
                    excluded.join("; ")
                )
            };
            return Err(ApiFailure::conflict(headline));
        }
        offered.retain(|candidate| {
            if let Some(note) = unhealthy_note(&candidate.candidate, &candidate.model.value) {
                unhealthy.push(note);
                false
            } else {
                true
            }
        });
        if offered.is_empty() {
            return Err(all_unhealthy(
                model,
                &unhealthy,
                source.profile_description(),
            ));
        }
        let prefer = backend.quota_preferences().await?;
        let power_ranking = backend.power_ranking().await?;
        let (anchor, placement) = match source {
            ModelProfileCandidateSource::Subagents { parent_profile } => (
                Some(parent_profile),
                if matches!(category, Some("supervisor" | "orchestrator")) {
                    Placement::Orchestrator
                } else {
                    Placement::Child
                },
            ),
            ModelProfileCandidateSource::Session { ranking_anchor } => {
                (ranking_anchor, Placement::TopLevel)
            }
            ModelProfileCandidateSource::Reviewers
            | ModelProfileCandidateSource::ReviewersReady => (None, Placement::Child),
        };
        let telemetry_store = backend.quota_telemetry_store();
        draw_context = Some((telemetry_store, prefer, power_ranking, anchor, placement));
    }
    let reviewer_source = matches!(
        source,
        ModelProfileCandidateSource::Reviewers | ModelProfileCandidateSource::ReviewersReady
    );
    let mut offered_any_effort = false;
    let mut selection_errors = Vec::new();
    while !offered.is_empty() {
        let reservation_slot =
            if let Some((store, prefer, ranking, anchor, placement)) = &draw_context {
                ReservationSlot::new(store.draw_and_reserve(
                    &mut offered,
                    execution_policy,
                    *placement,
                    category,
                    *anchor,
                    prefer,
                    ranking,
                    now_epoch_seconds(),
                )?)
            } else {
                ReservationSlot::default()
            };
        if offered.is_empty() {
            break;
        }
        let candidate = offered.remove(0);
        // A reviewer's effort is configured, not chosen: start with it as
        // configured and let the harness report a value it rejects, rather
        // than launching every candidate harness to list its efforts first.
        // Efforts already known for this model still pass over a profile
        // that cannot serve the slot, so a later one that can still runs it.
        if reviewer_source {
            let effort = match requirement {
                EffortRequirement::Exact(requested) => {
                    let known =
                        backend.cached_model_efforts(&candidate.profile_id, &candidate.model.value);
                    if let Some(known) = known
                        && !known.iter().any(|choice| choice.value == requested)
                    {
                        offered_any_effort = true;
                        selection_errors.push(format!(
                            "{} offers efforts: {}",
                            candidate.profile_id,
                            effort_names(&known)
                        ));
                        continue;
                    }
                    Some(requested.to_owned())
                }
                EffortRequirement::Default | EffortRequirement::Any => None,
            };
            let mut selected =
                selection(candidate.profile_id.clone(), &candidate.model.value, effort);
            selected.reservation = reservation_slot;
            return Ok(selected);
        }
        let result = model_efforts(backend, &candidate.candidate, &candidate.model.value).await;
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
        let mut selected = selection(candidate.profile_id.clone(), &candidate.model.value, effort);
        selected.reservation = reservation_slot;
        return Ok(selected);
        // Failed validation drops the tentative reservation before the next
        // draw, so concurrent placements are included in its telemetry.
    }
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
        reservation: ReservationSlot::default(),
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

/// "profile codex2 is rate-limited until 2026-10-25 14:05:00 UTC (...)" when the
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
    candidates: Vec<SubagentCandidate>,
    parent_profile: &str,
    model: &str,
    now_seconds: i64,
) -> Vec<SubagentCandidate> {
    struct Wrapped {
        candidate: SubagentCandidate,
        scheduled: ScheduledCandidate,
    }
    impl AsRef<ScheduledCandidate> for Wrapped {
        fn as_ref(&self) -> &ScheduledCandidate {
            &self.scheduled
        }
    }
    let telemetry = mj_accounts::quota_telemetry::QuotaTelemetrySnapshot::default();
    let power_ranking = mj_accounts::power_ranking::PowerRankingSnapshot::baseline();
    let ctx = SchedulingContext {
        execution_policy: ExecutionPolicy::Unconstrained,
        placement: Placement::Child,
        category: Some("straightforward"),
        ranking_anchor: Some(parent_profile),
        prefer: &[],
        telemetry: &telemetry,
        power_ranking,
        now_seconds,
    };
    let mut wrapped: Vec<Wrapped> = candidates
        .into_iter()
        .map(|candidate| {
            let scheduled = scheduled_candidate(&candidate, model, None);
            Wrapped {
                candidate,
                scheduled,
            }
        })
        .collect();
    rank_scheduled_candidates(&mut wrapped, &ctx);
    let mut candidates: Vec<SubagentCandidate> = wrapped.into_iter().map(|w| w.candidate).collect();
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
pub struct LegacySubagentSourceRange {
    pub file: PathBuf,
    pub start: u64,
    pub end: u64,
}

pub async fn build_legacy_subagent_prompt(
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
    fn rank_test_candidates(
        candidates: &mut Vec<SubagentCandidate>,
        ranking_anchor: Option<&str>,
        prefer: &[String],
        model: &str,
        now_seconds: i64,
        placement: Placement,
    ) {
        struct Wrapped {
            candidate: SubagentCandidate,
            scheduled: ScheduledCandidate,
        }
        impl AsRef<ScheduledCandidate> for Wrapped {
            fn as_ref(&self) -> &ScheduledCandidate {
                &self.scheduled
            }
        }
        let telemetry = mj_accounts::quota_telemetry::QuotaTelemetrySnapshot::default();
        let power_ranking = mj_accounts::power_ranking::PowerRankingSnapshot::baseline();
        let ctx = SchedulingContext {
            execution_policy: ExecutionPolicy::Unconstrained,
            placement,
            category: Some("straightforward"),
            ranking_anchor,
            prefer,
            telemetry: &telemetry,
            power_ranking,
            now_seconds,
        };
        let mut wrapped: Vec<Wrapped> = std::mem::take(candidates)
            .into_iter()
            .map(|candidate| {
                let scheduled = scheduled_candidate(&candidate, model, None);
                Wrapped {
                    candidate,
                    scheduled,
                }
            })
            .collect();
        rank_scheduled_candidates(&mut wrapped, &ctx);
        candidates.extend(wrapped.into_iter().map(|w| w.candidate));
    }

    fn draw_test_candidates(
        candidates: &mut Vec<SubagentCandidate>,
        ranking_anchor: Option<&str>,
        prefer: &[String],
        units: &[f64],
        model: &str,
        now_seconds: i64,
        placement: Placement,
    ) {
        struct Wrapped {
            candidate: SubagentCandidate,
            scheduled: ScheduledCandidate,
        }
        impl AsRef<ScheduledCandidate> for Wrapped {
            fn as_ref(&self) -> &ScheduledCandidate {
                &self.scheduled
            }
        }
        let telemetry = mj_accounts::quota_telemetry::QuotaTelemetrySnapshot::default();
        let power_ranking = mj_accounts::power_ranking::PowerRankingSnapshot::baseline();
        let ctx = SchedulingContext {
            execution_policy: ExecutionPolicy::Unconstrained,
            placement,
            category: Some("straightforward"),
            ranking_anchor,
            prefer,
            telemetry: &telemetry,
            power_ranking,
            now_seconds,
        };
        let mut wrapped: Vec<Wrapped> = std::mem::take(candidates)
            .into_iter()
            .map(|candidate| {
                let scheduled = scheduled_candidate(&candidate, model, None);
                Wrapped {
                    candidate,
                    scheduled,
                }
            })
            .collect();
        order_scheduled_candidates_by_draw(&mut wrapped, units, &ctx);
        candidates.extend(wrapped.into_iter().map(|w| w.candidate));
    }

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
            telemetry_store: None,
            quota_preferences: Vec::new(),
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
                    windows: vec![mj_accounts::quota::QuotaWindow {
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
            may_consume_credits: false,
            unhealthy: Default::default(),
            cache_identity: None,
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
        rank_test_candidates(
            &mut candidates,
            Some("parent"),
            &[],
            "luna",
            0,
            Placement::Child,
        );
        assert_eq!(
            ids(&candidates),
            vec!["high", "parent", "a-other", "b-other", "unknown", "api"]
        );
    }

    // A top-level launch ranks a profile inside a five-hour window below
    // unwindowed and unknown quota but above paid credits and usage-priced
    // profiles, which bill per request; a five-hour window that has already
    // reset, or an idle one with no reset time, is not in force. Child placements keep the plain quota order.

    #[test]
    fn load_balancing_preferences_lead_among_quota_equals_in_list_order() {
        let mut candidates = vec![
            candidate("a-plain", Some(50), &["luna"]),
            candidate("b-preferred", Some(50), &["luna"]),
            candidate("c-preferred-first", Some(50), &["luna"]),
        ];
        rank_test_candidates(
            &mut candidates,
            None,
            &["c-preferred-first".into(), "b-preferred".into()],
            "luna",
            0,
            Placement::Child,
        );
        assert_eq!(
            ids(&candidates),
            ["c-preferred-first", "b-preferred", "a-plain"]
        );
        // An exhausted preference does not win: quota still owns feasibility.
        let mut candidates = vec![
            candidate("plain", Some(50), &["luna"]),
            candidate("preferred-out", Some(0), &["luna"]),
        ];
        rank_test_candidates(
            &mut candidates,
            None,
            &["preferred-out".into()],
            "luna",
            0,
            Placement::Child,
        );
        assert_eq!(ids(&candidates), ["plain"]);
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
        windows.push(mj_accounts::quota::QuotaWindow {
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
        rank_test_candidates(
            &mut for_fable,
            None,
            &[],
            "claude-fable-5-1",
            1_000,
            Placement::Child,
        );
        assert_eq!(ids(&for_fable), ["other"]);

        let mut for_opus = vec![fable, other];
        rank_test_candidates(
            &mut for_opus,
            None,
            &[],
            "claude-opus-5",
            1_000,
            Placement::Child,
        );
        assert_eq!(ids(&for_opus), ["fable-profile", "other"]);
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
                profile.may_consume_credits = !included;
                profiles.push(profile);
            }
            let draws = 40_000;
            let mut first = std::collections::BTreeMap::<String, u32>::new();
            for _ in 0..draws {
                let mut candidates = profiles.clone();
                let units = (0..candidates.len()).map(|_| unit()).collect::<Vec<_>>();
                draw_test_candidates(
                    &mut candidates,
                    Some("thirty-b"),
                    &[],
                    &units,
                    "luna",
                    0,
                    Placement::Child,
                );
                let chosen = if included {
                    assert_eq!(ids(&candidates)[3..], ["unknown", "api"]);
                    &candidates[0]
                } else {
                    assert_eq!(candidates[0].profile_id, "unknown");
                    assert_eq!(ids(&candidates)[4..], ["api"]);
                    &candidates[1]
                };
                *first.entry(chosen.profile_id.clone()).or_default() += 1;
            }
            let share = |id: &str| f64::from(first[id]) / f64::from(draws);
            if included && !price_weighted {
                // Included subscriptions with equal prices and 60% vs 30% vs 30% remaining
                // use 1% reserve floor and pace exponent alpha = 2.0:
                // 59^2 : 29^2 : 29^2 => 0.6742 : 0.1629 : 0.1629.
                let w60 = 59.0_f64.powi(2);
                let w30 = 29.0_f64.powi(2);
                let total = w60 + 2.0 * w30;
                assert!((share("sixty") - w60 / total).abs() < 0.01, "{first:?}");
                assert!((share("thirty-a") - w30 / total).abs() < 0.01, "{first:?}");
                assert!((share("thirty-b") - w30 / total).abs() < 0.01, "{first:?}");
            } else {
                assert!((share("sixty") - 0.5).abs() < 0.01, "{first:?}");
                assert!((share("thirty-a") - 0.25).abs() < 0.01, "{first:?}");
                assert!((share("thirty-b") - 0.25).abs() < 0.01, "{first:?}");
            }
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
            telemetry_store: None,
            quota_preferences: Vec::new(),
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
                    ExecutionPolicy::Unconstrained,
                    None,
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
        telemetry_store: Option<mj_accounts::quota_telemetry::QuotaTelemetryStore>,
        quota_preferences: Vec<String>,
        config: mj_core::config::SubagentConfig,
        candidates: SubagentCandidates,
    }

    impl SubagentBackend for FakeSelectionBackend {
        fn session_profile_candidates(&self) -> BoxFuture<'_, anyhow::Result<SubagentCandidates>> {
            let candidates = self.candidates.clone();
            Box::pin(async move { Ok(candidates) })
        }

        fn cached_model_efforts(
            &self,
            profile: &str,
            model: &str,
        ) -> Option<Vec<mj_core::acp::SessionConfigChoice>> {
            match profile {
                "cached-low" => Some(vec![mj_core::acp::SessionConfigChoice {
                    value: "low".into(),
                    name: "Low".into(),
                    description: None,
                }]),
                // A completed discovery that found no effort choices.
                "cached-none" => Some(Vec::new()),
                _ => self
                    .candidates
                    .offered
                    .iter()
                    .find(|candidate| candidate.profile_id == profile)
                    .map(|candidate| &candidate.choices)
                    .filter(|choices| {
                        choices.model.as_deref() == Some(model) && !choices.efforts.is_empty()
                    })
                    .map(|choices| choices.efforts.clone()),
            }
        }

        fn quota_telemetry_store(&self) -> mj_accounts::quota_telemetry::QuotaTelemetryStore {
            self.telemetry_store.clone().unwrap_or_default()
        }

        fn quota_preferences(&self) -> BoxFuture<'_, AnyResult<Vec<String>>> {
            let preferences = self.quota_preferences.clone();
            Box::pin(async move { Ok(preferences) })
        }

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
                if profile.starts_with("probe-fails-") {
                    if let Some(store) = &self.telemetry_store {
                        let now = now_epoch_seconds();
                        for index in 0..30 {
                            store.record_committed_placement(
                                &format!("concurrent-{index}"),
                                "runway-one",
                                "subtle",
                                now,
                            );
                        }
                    }
                    anyhow::bail!("model-specific probe failed for this profile");
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
                        .map(|candidate| &candidate.profile_id)
                        .chain(candidates.unavailable.iter().map(|(id, _)| id))
                        .any(|id| !tried.contains(id))
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
        fn export_patch(
            &self,
            _session_id: String,
        ) -> BoxFuture<'_, Result<ExportPatch, ExportError>> {
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
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: SubagentConfig {
                straightforward: vec![choice.clone()],
                subtle: vec![choice],
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
                orchestrators: false,
                straightforward: None,
                subtle: None,
            },
            SubagentTaskType::Straightforward,
            ExecutionPolicy::Unconstrained,
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
    async fn redraw_after_failed_discovery_rechecks_runway_against_concurrent_reservations() {
        let now = now_epoch_seconds();
        let reset = now + 3 * 86_400;
        let mut offered = vec![
            candidate("probe-fails-zero", Some(99), &["gpt-6.1-sol"]),
            candidate("runway-one", Some(10), &["gpt-6.1-sol"]),
            candidate("healthy-two", Some(60), &["gpt-6.1-sol"]),
        ];
        for (index, candidate) in offered.iter_mut().enumerate() {
            let CandidateQuota::Subscription { windows, .. } = &mut candidate.quota else {
                unreachable!("fixture has subscription quota");
            };
            windows[0].duration_seconds = Some(604_800);
            windows[0].resets_at_epoch_seconds = Some(reset);
            if index > 0 {
                candidate.choices.efforts = efforts(&["high"]);
            }
        }
        let telemetry_store = mj_accounts::quota_telemetry::QuotaTelemetryStore::new();
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: Some(telemetry_store.clone()),
            // Force `probe-fails-zero` first despite weighted tier-0 sampling,
            // and prefer `runway-one` over `healthy-two` so a stale-telemetry
            // redraw would deterministically pick `runway-one`.
            quota_preferences: vec!["probe-fails-zero".into(), "runway-one".into()],
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: offered.clone(),
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_model_profile_selection_from(
            &backend,
            SubagentCandidates {
                offered,
                unavailable: Vec::new(),
            },
            None,
            "gpt-6.1-sol",
            EffortRequirement::Exact("high"),
            None,
            ModelProfileCandidateSource::Subagents {
                parent_profile: "probe-fails-zero",
            },
            ExecutionPolicy::Unconstrained,
            Some("subtle"),
        )
        .await
        .unwrap();
        assert_eq!(
            telemetry_store
                .snapshot()
                .recent_placements
                .get("runway-one")
                .map_or(0, Vec::len),
            30,
            "probe-fails-zero must be probed first so the redraw sees the concurrent placements"
        );
        assert_eq!(selected.profile_id, "healthy-two");
    }

    #[tokio::test]
    async fn reviewer_uses_ready_profile_without_waiting_for_pending_discovery() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: true,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![candidate("ready", Some(70), &["fixed"])],
                unavailable: Vec::new(),
            },
        });
        let selected = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            resolve_reviewer_model_profile_selection(
                &backend,
                "fixed",
                Some("high"),
                ExecutionPolicy::Unconstrained,
                "reviewer",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(selected.profile_id, "ready");
    }

    /// A profile refused for the model must not hide another profile whose
    /// capabilities could not be checked.
    #[tokio::test]
    async fn reviewer_refusal_still_lists_profiles_that_could_not_be_checked() {
        let mut rate_limited = candidate("rate-limited", Some(70), &["fixed"]);
        rate_limited.unhealthy.insert(
            "fixed".into(),
            mj_accounts::profile_health::Unhealthy {
                fault: mj_accounts::profile_health::ProfileFault::RateLimited,
                until: std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_792_937_100),
                message: "exceeded retry limit, last status: 429".into(),
            },
        );
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: Some(vec![
                (
                    0,
                    SubagentCandidates {
                        offered: vec![rate_limited],
                        unavailable: Vec::new(),
                    },
                ),
                (
                    0,
                    SubagentCandidates {
                        offered: Vec::new(),
                        unavailable: vec![("signed-out".into(), "capability probe failed".into())],
                    },
                ),
            ]),
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates::default(),
        });
        let error = resolve_reviewer_model_profile_selection(
            &backend,
            "fixed",
            None,
            ExecutionPolicy::Unconstrained,
            "reviewer",
        )
        .await
        .unwrap_err();
        assert!(
            error
                .message
                .contains("rate-limited until 2026-10-25 14:05:00 UTC"),
            "{}",
            error.message
        );
        assert!(
            error
                .message
                .contains("Could not check: signed-out (capability probe failed)"),
            "{}",
            error.message
        );
    }

    /// A profile whose ID is the tail of an already listed one is still listed.
    #[tokio::test]
    async fn reviewer_refusal_lists_an_unchecked_profile_named_like_a_listed_one() {
        let stage = |offered: Vec<SubagentCandidate>, unavailable: &[(&str, &str)]| {
            (
                0,
                SubagentCandidates {
                    offered,
                    unavailable: unavailable
                        .iter()
                        .map(|(id, reason)| ((*id).into(), (*reason).into()))
                        .collect(),
                },
            )
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: Some(vec![
                stage(
                    vec![candidate("other", Some(70), &["flash"])],
                    &[("team-terra", "probe failed")],
                ),
                stage(Vec::new(), &[("terra", "login expired")]),
            ]),
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates::default(),
        });
        let error = resolve_reviewer_model_profile_selection(
            &backend,
            "fixed",
            None,
            ExecutionPolicy::Unconstrained,
            "reviewer",
        )
        .await
        .unwrap_err();
        assert!(
            error.message.contains("team-terra (probe failed)")
                && error.message.contains("terra (login expired)"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn legacy_pinned_reviewer_keeps_its_profile_when_another_offers_the_model() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
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
            telemetry_store: None,
            quota_preferences: Vec::new(),
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
            ExecutionPolicy::Unconstrained,
            None,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "subscription");
    }

    // Hard-won: a stalled effort probe refused a reviewer whose effort was configured.
    #[tokio::test(start_paused = true)]
    async fn reviewer_configured_or_default_effort_skips_stalled_model_effort_probe() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![candidate("stalled-default", Some(80), &["fixed"])],
                unavailable: Vec::new(),
            },
        });
        for (requirement, effort) in [
            (EffortRequirement::Default, None),
            (EffortRequirement::Exact("high"), Some("high")),
        ] {
            let start = tokio::time::Instant::now();
            let selected = resolve_model_profile_selection_matching(
                &backend,
                None,
                "fixed",
                requirement,
                None,
                ModelProfileCandidateSource::ReviewersReady,
                ExecutionPolicy::Unconstrained,
                None,
            )
            .await
            .unwrap();
            assert_eq!(selected.profile_id, "stalled-default");
            assert_eq!(selected.effort.as_deref(), effort);
            assert_eq!(tokio::time::Instant::now(), start);
        }

        // A preferred profile known not to offer the configured effort yields
        // the slot to one whose efforts are unknown, still without probing,
        // whether the efforts are known from its default model or from a
        // completed discovery of this model.
        let mut low_only = candidate("low-only", Some(90), &["fixed"]);
        low_only.choices.efforts = vec![mj_core::acp::SessionConfigChoice {
            value: "low".into(),
            name: "Low".into(),
            description: None,
        }];
        let cached_low = candidate("cached-low", Some(80), &["other", "fixed"]);
        let cached_none = candidate("cached-none", Some(80), &["other", "fixed"]);
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    low_only,
                    cached_low,
                    cached_none,
                    candidate("unknown", Some(10), &["fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        // The draw is random; repeat so a profile wrongly kept is drawn.
        for _ in 0..24 {
            let start = tokio::time::Instant::now();
            let selected = resolve_model_profile_selection_matching(
                &backend,
                None,
                "fixed",
                EffortRequirement::Exact("high"),
                None,
                ModelProfileCandidateSource::ReviewersReady,
                ExecutionPolicy::Unconstrained,
                None,
            )
            .await
            .unwrap();
            assert_eq!(selected.profile_id, "unknown");
            assert_eq!(tokio::time::Instant::now(), start);
        }
    }

    #[tokio::test]
    async fn reviewer_selection_shares_spawn_effort_health_and_draw_rules() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("first", Some(70), &["no-effort", "fixed"]),
                    candidate("second", Some(70), &["no-effort", "fixed"]),
                ],
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_reviewer_model_profile_selection(
            &backend,
            "fixed",
            Some("high"),
            ExecutionPolicy::Unconstrained,
            "reviewer",
        )
        .await
        .unwrap();
        assert_eq!(selected.effort.as_deref(), Some("high"));
        let error = resolve_reviewer_model_profile_selection(
            &backend,
            "missing",
            None,
            ExecutionPolicy::Unconstrained,
            "reviewer",
        )
        .await
        .unwrap_err();
        assert!(
            error.message.contains("no enabled profile"),
            "{}",
            error.message
        );
        let mut saw_different_profiles = false;
        for _ in 0..48 {
            let first = resolve_reviewer_model_profile_selection(
                &backend,
                "no-effort",
                None,
                ExecutionPolicy::Unconstrained,
                "reviewer",
            )
            .await
            .unwrap();
            let second = resolve_reviewer_model_profile_selection(
                &backend,
                "no-effort",
                None,
                ExecutionPolicy::Unconstrained,
                "reviewer",
            )
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
                telemetry_store: None,
                quota_preferences: Vec::new(),
                config: SubagentConfig {
                    straightforward: vec![SubagentChoice {
                        model: "sonnet".into(),
                        effort: Some("high".into()),
                    }],
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
                    std::slice::from_ref(&SubagentChoice {
                        model: request.into(),
                        effort: Some("high".into()),
                    }),
                    ExecutionPolicy::Unconstrained,
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
                let reviewer = resolve_reviewer_model_profile_selection(
                    &backend,
                    request,
                    Some("high"),
                    ExecutionPolicy::Unconstrained,
                    "reviewer",
                )
                .await
                .unwrap();
                assert_eq!(reviewer, selected);
            }
            let selected = resolve_subagent_policy_selection(
                &backend,
                "parent",
                &SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
                SubagentTaskType::Straightforward,
                ExecutionPolicy::Unconstrained,
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
    async fn multi_choice_category_expands_across_configured_peers() {
        use mj_core::config::{SubagentChoice, SubagentConfig};
        use mj_core::subagent::{SubagentPolicy, SubagentTaskType};
        let choice = |model: &str| SubagentChoice {
            model: model.into(),
            effort: None,
        };
        let config = SubagentConfig {
            straightforward: vec![choice("claude-sonnet-4-6"), choice("gpt-5.4-mini")],
            ..Default::default()
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config,
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("claude-exhausted", Some(0), &["claude-sonnet-4-6"]),
                    candidate("codex-ready", Some(80), &["gpt-5.4-mini"]),
                ],
                unavailable: vec![],
            },
        });
        let policy = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        };
        let selected = resolve_subagent_policy_selection(
            &backend,
            "claude-exhausted",
            &policy,
            SubagentTaskType::Straightforward,
            ExecutionPolicy::Unconstrained,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "codex-ready");
        assert_eq!(selected.model, "gpt-5.4-mini");
    }

    #[tokio::test]
    async fn task_type_spawn_uses_global_choices_and_quota_ranked_enabled_profiles() {
        use mj_core::config::{SubagentChoice, SubagentConfig};
        use mj_core::subagent::{SubagentPolicy, SubagentTaskType};
        let candidates = SubagentCandidates {
            offered: vec![
                // No quota left, so the weighted draw never puts it first.
                candidate("parent", None, &["fixed", "no-effort"]),
                candidate("best", Some(70), &["fixed", "no-effort"]),
                candidate("full-but-wrong-effort", Some(100), &["fixed"]),
            ],
            unavailable: vec![],
        };
        let config = SubagentConfig {
            straightforward: vec![SubagentChoice {
                model: "no-effort".into(),
                effort: None,
            }],
            subtle: vec![SubagentChoice {
                model: "fixed".into(),
                effort: Some("high".into()),
            }],
            ..Default::default()
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
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
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
                task_type,
                ExecutionPolicy::Unconstrained,
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
                        mj_accounts::profile_health::Unhealthy {
                            fault: mj_accounts::profile_health::ProfileFault::RateLimited,
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
                telemetry_store: None,
                quota_preferences: Vec::new(),
                config: config.clone(),
                candidates,
            })
        };
        let any_policy = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        };
        let selected = resolve_subagent_policy_selection(
            &marked(&["best"]),
            "parent",
            &any_policy,
            SubagentTaskType::Subtle,
            ExecutionPolicy::Unconstrained,
        )
        .await
        .unwrap();
        assert_eq!(selected.profile_id, "parent");
        let refused = resolve_subagent_policy_selection(
            &marked(&["best", "parent"]),
            "parent",
            &any_policy,
            SubagentTaskType::Subtle,
            ExecutionPolicy::Unconstrained,
        )
        .await
        .unwrap_err();
        assert!(
            refused.message.contains(
                "profile best is rate-limited until 2026-10-25 14:05:00 UTC (exceeded retry limit, last status: 429)"
            ),
            "{}",
            refused.message
        );
        let unconfigured: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates,
        });
        let mixed_policy = SubagentPolicy::Mjolnir {
            orchestrators: false,
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
            let selected = resolve_subagent_policy_selection(
                &backend,
                "parent",
                &mixed_policy,
                task_type,
                ExecutionPolicy::Unconstrained,
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
        let error = resolve_subagent_policy_selection(
            &unconfigured,
            "parent",
            &SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: None,
                subtle: None,
            },
            SubagentTaskType::Subtle,
            ExecutionPolicy::Unconstrained,
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
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: SubagentConfig {
                straightforward: vec![SubagentChoice {
                    model: "no-effort".into(),
                    effort: None,
                }],
                ..Default::default()
            },
            candidates: SubagentCandidates {
                offered: vec![candidate("best", Some(70), &["no-effort"])],
                unavailable: vec![],
            },
        });
        let policy = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        };
        let error = resolve_subagent_policy_selection(
            &straightforward_only,
            "parent",
            &policy,
            SubagentTaskType::Subtle,
            ExecutionPolicy::Unconstrained,
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
            ExecutionPolicy::Unconstrained,
        )
        .await
        .unwrap();
        assert_eq!(selected.model, "no-effort");
    }

    #[tokio::test]
    async fn unhealthy_higher_value_model_on_shared_pool_falls_back_to_healthy_sibling() {
        let mut unhealthy_map = std::collections::BTreeMap::new();
        unhealthy_map.insert(
            "gpt-6.1-luna".to_owned(),
            mj_accounts::profile_health::Unhealthy {
                fault: mj_accounts::profile_health::ProfileFault::RateLimited,
                message: "rate limited".to_owned(),
                until: std::time::SystemTime::now() + std::time::Duration::from_secs(300),
            },
        );
        let mut shared_profile = candidate("codex1", Some(80), &["gpt-6.1-luna", "gpt-6.1-sol"]);
        shared_profile.unhealthy = unhealthy_map;
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: mj_core::config::SubagentConfig {
                straightforward: vec![
                    mj_core::config::SubagentChoice {
                        model: "gpt-6.1-luna".to_owned(),
                        effort: None,
                    },
                    mj_core::config::SubagentChoice {
                        model: "gpt-6.1-sol".to_owned(),
                        effort: None,
                    },
                ],
                ..Default::default()
            },
            candidates: SubagentCandidates {
                offered: vec![shared_profile],
                unavailable: Vec::new(),
            },
        });
        let selected = resolve_subagent_policy_selection(
            &backend,
            "parent",
            &mj_core::subagent::SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: None,
                subtle: None,
            },
            mj_core::subagent::SubagentTaskType::Straightforward,
            ExecutionPolicy::Unconstrained,
        )
        .await
        .expect("healthy sibling model on the same pool must still be selected");
        assert_eq!(selected.profile_id, "codex1");
        assert_eq!(selected.model, "gpt-6.1-sol");
    }

    #[tokio::test]
    async fn resolve_provider_session_selection_filters_to_provider_and_load_balances() {
        let mut config = mj_core::config::Config::default();
        config.profiles.insert(
            "claude-low".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Claude,
                std::path::PathBuf::from("/profiles/claude-low"),
            ),
        );
        config.profiles.insert(
            "claude-high".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Claude,
                std::path::PathBuf::from("/profiles/claude-high"),
            ),
        );
        config.profiles.insert(
            "kimi-full".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Kimi,
                std::path::PathBuf::from("/profiles/kimi-full"),
            ),
        );

        let make_candidate = |id: &str, harness: mj_core::config::HarnessKind, remaining: u8| {
            let mut c = candidate(id, Some(remaining), &["default-model"]);
            c.harness = harness;
            c
        };
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            ready_first: false,
            staged: None,
            discovery_fails: false,
            telemetry_store: None,
            quota_preferences: Vec::new(),
            config: Default::default(),
            candidates: SubagentCandidates {
                offered: vec![
                    make_candidate("claude-low", mj_core::config::HarnessKind::Claude, 0),
                    make_candidate("claude-high", mj_core::config::HarnessKind::Claude, 80),
                    make_candidate("kimi-full", mj_core::config::HarnessKind::Kimi, 100),
                ],
                unavailable: Vec::new(),
            },
        });

        let selected = resolve_provider_session_selection(
            &backend,
            &config,
            &mj_core::config::QuotaProvider::Anthropic,
            ExecutionPolicy::Unconstrained,
            None,
        )
        .await
        .expect("Anthropic should pick the non-exhausted Claude profile");
        assert_eq!(selected.profile_id, "claude-high");

        let missing = resolve_provider_session_selection(
            &backend,
            &config,
            &mj_core::config::QuotaProvider::Google,
            ExecutionPolicy::Unconstrained,
            None,
        )
        .await
        .err()
        .expect("provider with no enabled profiles must fail");
        assert!(
            missing.message.contains("Google"),
            "unexpected error: {}",
            missing.message
        );
    }
}
