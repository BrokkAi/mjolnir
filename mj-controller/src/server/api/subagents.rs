use super::*;

use mj_core::subagent::CURRENT_MODEL;

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
    if !matches!(parent.harness_kind.as_str(), "claude" | "codex") {
        return Err(ApiFailure::conflict(
            "only Claude and Codex sessions can spawn sub-agents",
        ));
    }
    let initial_prompt = build_subagent_prompt(&request.instructions)?;
    if request.task_name.trim().is_empty() {
        return Err(ApiFailure::bad_request("task_name cannot be empty"));
    }
    let selection = resolve_subagent_policy_selection(
        &backend,
        &parent_session_id,
        &parent.profile_id,
        &parent.subagents,
        request.profile_id.as_deref(),
        request.model.as_deref(),
        request.effort.as_deref(),
    )
    .await?;

    let relation = backend
        .start_subagent(crate::controller::RegisterSubagentRequest {
            parent_session_id: parent_session_id.clone(),
            task_name: request.task_name,
            profile_id: selection.profile_id,
            model: Some(selection.model.clone()),
            effort: selection.effort.clone(),
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
    backend
        .start_followup(
            relation.child_session_id.clone(),
            // Registration completes the first prompt (it names the handback
            // tool when the child gets one), so send what it kept.
            StartFollowup {
                model: Some(selection.model),
                effort: selection.effort,
                prompt: Some(relation.initial_prompt.clone()),
                fast_mode: selection.fast_mode,
            },
        )
        .await?;
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_subagent_policy_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_session_id: &str,
    parent_profile: &str,
    policy: &mj_core::subagent::SubagentPolicy,
    profile_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    use mj_core::subagent::SubagentPolicy;
    match policy {
        SubagentPolicy::AllModels => {
            resolve_subagent_selection(
                backend,
                parent_session_id,
                parent_profile,
                profile_id,
                model,
                effort,
            )
            .await
        }
        SubagentPolicy::SingleModel {
            model: fixed_model,
            effort: fixed_effort,
        } => {
            if profile_id.is_some() || model.is_some() || effort.is_some() {
                return Err(ApiFailure::bad_request(
                    "single-model spawn does not accept profile_id, model, or effort",
                ));
            }
            let effort_requirement = fixed_effort
                .as_deref()
                .map(EffortRequirement::Exact)
                .unwrap_or(EffortRequirement::NoChoices);
            resolve_model_profile_selection_matching(
                backend,
                parent_profile,
                None,
                fixed_model,
                effort_requirement,
                None,
            )
            .await
            .map_err(|error| {
                if error.status == StatusCode::BAD_REQUEST {
                    ApiFailure::conflict(format!(
                        "No eligible profile offers the fixed subagent model {fixed_model:?} with effort {fixed_effort:?}. {}",
                        error.message
                    ))
                } else {
                    error
                }
            })
        }
        _ => Err(ApiFailure::conflict(
            "this session does not allow Mjolnir sub-agents",
        )),
    }
}

/// Settle a spawn's profile, model and effort; both spawn paths use this.
///
/// The model is required, and [`CURRENT_MODEL`] names the parent's own. Unless
/// the caller pins a profile, the child runs on the eligible profile that
/// offers the model and has the most quota left, so a parent whose own login
/// is nearly spent does not give its child the same empty allowance. An
/// omitted effort follows the parent's when the chosen profile offers it.
pub(crate) async fn resolve_subagent_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_session_id: &str,
    parent_profile: &str,
    profile_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            ApiFailure::bad_request(format!(
                "spawn needs a model: name one from list_profiles, or \"{CURRENT_MODEL}\" for \
                 this session's own model"
            ))
        })?;
    let parent_config = if model == CURRENT_MODEL || effort.is_none() {
        backend
            .session_handle(parent_session_id.to_owned())
            .await?
            .and_then(|handle| handle.view().snapshot)
            .map(|snapshot| snapshot.operational.config.clone())
            .unwrap_or_default()
    } else {
        std::collections::BTreeMap::new()
    };
    let model = if model == CURRENT_MODEL {
        parent_config.get("model").cloned().ok_or_else(|| {
            ApiFailure::conflict(format!(
                "this session's current model is unknown; name a model from list_profiles \
                 instead of \"{CURRENT_MODEL}\""
            ))
        })?
    } else {
        model.to_owned()
    };
    resolve_model_profile_selection(
        backend,
        parent_profile,
        profile_id,
        &model,
        effort,
        parent_config.get("effort").map(String::as_str),
    )
    .await
}

/// Choose the eligible profile for an exact model and validate its effort.
/// New sessions and named-model sub-agent spawns share this quota-ranked path.
pub(crate) async fn resolve_model_profile_selection(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    profile_id: Option<&str>,
    model: &str,
    effort: Option<&str>,
    inherited_effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let requirement = effort
        .map(EffortRequirement::Exact)
        .unwrap_or(EffortRequirement::Any);
    resolve_model_profile_selection_matching(
        backend,
        parent_profile,
        profile_id,
        model,
        requirement,
        inherited_effort,
    )
    .await
}

#[derive(Clone, Copy)]
enum EffortRequirement<'a> {
    Any,
    NoChoices,
    Exact(&'a str),
}

async fn resolve_model_profile_selection_matching(
    backend: &Arc<dyn SubagentBackend>,
    parent_profile: &str,
    profile_id: Option<&str>,
    model: &str,
    requirement: EffortRequirement<'_>,
    inherited_effort: Option<&str>,
) -> Result<SubagentSelection, ApiFailure> {
    let candidates = backend
        .subagent_candidates(parent_profile.to_owned())
        .await?;
    if matches!(requirement, EffortRequirement::Any) {
        let chosen = choose_subagent_profile(candidates, profile_id, parent_profile, model)?;
        let efforts = model_efforts(backend, &chosen, model).await?;
        let effort = child_effort(model, &efforts, None, inherited_effort)?;
        return Ok(selection(chosen.profile_id, model, effort));
    }

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
            return Err(ApiFailure::bad_request(format!(
                "profile {requested:?} is not eligible for sub-agent use from this session"
            )));
        }
    } else {
        rank_candidates(&mut offered, parent_profile);
    }
    let all_offered = offered.clone();
    let mut matching_model = false;
    let mut offered_any_effort = false;
    let mut selection_errors = Vec::new();
    for candidate in offered {
        if !offers_model(&candidate, model) {
            continue;
        }
        matching_model = true;
        let efforts = match model_efforts(backend, &candidate, model).await {
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
            EffortRequirement::NoChoices if efforts.is_empty() => None,
            EffortRequirement::Exact(_) => {
                selection_errors.push(format!(
                    "{} offers efforts: {}",
                    candidate.profile_id,
                    effort_names(&efforts)
                ));
                continue;
            }
            EffortRequirement::NoChoices => {
                selection_errors.push(format!(
                    "{} offers efforts: {}",
                    candidate.profile_id,
                    effort_names(&efforts)
                ));
                continue;
            }
            EffortRequirement::Any => unreachable!("handled above"),
        };
        return Ok(selection(candidate.profile_id, model, effort));
    }
    if !matching_model {
        return Err(ApiFailure::bad_request(no_profile_offers(
            model,
            &all_offered,
            &unavailable,
        )));
    }
    let required = match requirement {
        EffortRequirement::Exact(effort) => format!("effort {effort:?}"),
        EffortRequirement::NoChoices => "no effort choices".to_owned(),
        EffortRequirement::Any => unreachable!("handled above"),
    };
    let unavailable_effort = match requirement {
        EffortRequirement::Exact(effort) if !offered_any_effort => format!(
            "model {model:?} offers no effort choices; requested effort {effort:?} is unavailable. "
        ),
        _ => format!("no eligible profile offers model {model:?} with {required}. "),
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
        None => Ok(parent.filter(|effort| offers(effort)).map(str::to_owned)),
    }
}

/// The profile a child runs on. A pinned profile must be a candidate that
/// offers the model; otherwise the best-ranked candidate that offers it wins.
pub(crate) fn choose_subagent_profile(
    candidates: SubagentCandidates,
    requested_profile: Option<&str>,
    parent_profile: &str,
    model: &str,
) -> Result<SubagentCandidate, ApiFailure> {
    let SubagentCandidates {
        mut offered,
        unavailable,
    } = candidates;
    if let Some(requested) = requested_profile {
        if let Some((_, reason)) = unavailable.iter().find(|(id, _)| id == requested) {
            return Err(ApiFailure::conflict(format!(
                "profile {requested:?} is unavailable: {reason}"
            )));
        }
        let chosen = offered
            .into_iter()
            .find(|candidate| candidate.profile_id == requested)
            .ok_or_else(|| {
                ApiFailure::bad_request(format!(
                    "profile {requested:?} is not eligible for sub-agent use from this session"
                ))
            })?;
        validate_selectors(&chosen.choices, Some(model), None)?;
        return Ok(chosen);
    }
    rank_candidates(&mut offered, parent_profile);
    match offered
        .iter()
        .position(|candidate| offers_model(candidate, model))
    {
        Some(index) => Ok(offered.swap_remove(index)),
        None => Err(ApiFailure::bad_request(no_profile_offers(
            model,
            &offered,
            &unavailable,
        ))),
    }
}

/// Order candidates best first: the most quota left, with unknown quota last;
/// then the parent's own profile; then profile id, so the order never depends
/// on how the configuration happens to list them.
pub(crate) fn rank_candidates(candidates: &mut [SubagentCandidate], parent_profile: &str) {
    candidates.sort_by(|left, right| {
        right
            .remaining_percent
            .cmp(&left.remaining_percent)
            .then_with(|| {
                (left.profile_id != parent_profile).cmp(&(right.profile_id != parent_profile))
            })
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
}

/// The candidates `list_profiles` shows: ranked, with profiles of one harness
/// that offer exactly the same models merged into the best-ranked of them.
/// Several logins of one account type become one entry, and a profile that
/// offers different models is never hidden behind another.
pub(crate) fn merge_same_models(
    mut candidates: Vec<SubagentCandidate>,
    parent_profile: &str,
) -> Vec<SubagentCandidate> {
    rank_candidates(&mut candidates, parent_profile);
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

fn offers_model(candidate: &SubagentCandidate, model: &str) -> bool {
    candidate
        .choices
        .models
        .iter()
        .any(|choice| choice.value == model)
}

/// A refusal the parent can act on: which models each eligible profile does
/// offer, and which profiles could not be checked.
fn no_profile_offers(
    model: &str,
    offered: &[SubagentCandidate],
    unavailable: &[(String, String)],
) -> String {
    let mut message = format!("no eligible profile offers model {model:?}.");
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
            remaining_percent: remaining,
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
    fn ranking_puts_most_quota_first_unknown_last_and_ties_to_the_parent() {
        let mut candidates = vec![
            candidate("unknown", None, &["luna"]),
            candidate("b-other", Some(40), &["luna"]),
            candidate("parent", Some(40), &["luna"]),
            candidate("a-other", Some(40), &["luna"]),
            candidate("high", Some(90), &["luna"]),
            candidate("empty", Some(0), &["luna"]),
        ];
        rank_candidates(&mut candidates, "parent");
        assert_eq!(
            ids(&candidates),
            vec!["high", "parent", "a-other", "b-other", "empty", "unknown"]
        );
    }

    #[test]
    fn a_pinned_profile_must_be_eligible_and_offer_the_model() {
        let candidates = || SubagentCandidates {
            offered: vec![candidate("codex", Some(5), &["luna"])],
            unavailable: vec![("glm".to_owned(), "not signed in".to_owned())],
        };
        assert_eq!(
            choose_subagent_profile(candidates(), Some("codex"), "codex", "luna")
                .unwrap()
                .profile_id,
            "codex"
        );
        let not_offered =
            choose_subagent_profile(candidates(), Some("codex"), "codex", "sol").unwrap_err();
        assert!(
            not_offered.message.contains("does not offer \"sol\""),
            "{}",
            not_offered.message
        );
        let ineligible =
            choose_subagent_profile(candidates(), Some("kimi"), "codex", "luna").unwrap_err();
        assert!(
            ineligible.message.contains("not eligible"),
            "{}",
            ineligible.message
        );
        let broken =
            choose_subagent_profile(candidates(), Some("glm"), "codex", "luna").unwrap_err();
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
        );
        assert_eq!(ids(&merged), vec!["deepseek", "codex4", "codex3"]);
    }

    /// Just enough of a backend to drive `resolve_subagent_selection`: one
    /// profile's candidates, and no live parent session (so effort inherits
    /// nothing and the parent's real model is never consulted).
    struct FakeSelectionBackend {
        candidates: SubagentCandidates,
    }

    impl SubagentBackend for FakeSelectionBackend {
        fn profile_config(
            &self,
            profile: String,
            model: Option<String>,
            _refresh: bool,
        ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
            Box::pin(async move {
                let mut choices = self
                    .candidates
                    .offered
                    .iter()
                    .find(|candidate| candidate.profile_id == profile)
                    .unwrap()
                    .choices
                    .clone();
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
            _role: Option<mj_core::transcript::TranscriptRole>,
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
        ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>> {
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

    #[tokio::test]
    async fn fixed_spawn_ranks_only_profiles_supporting_the_exact_model_and_effort() {
        use mj_core::subagent::SubagentPolicy;
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            candidates: SubagentCandidates {
                offered: vec![
                    candidate("parent", Some(2), &["fixed"]),
                    candidate("best", Some(70), &["fixed"]),
                    candidate("full-but-wrong-effort", Some(100), &["fixed"]),
                ],
                unavailable: vec![],
            },
        });
        let policy = SubagentPolicy::SingleModel {
            model: "fixed".into(),
            effort: Some("high".into()),
        };
        let selected =
            resolve_subagent_policy_selection(&backend, "s", "parent", &policy, None, None, None)
                .await
                .unwrap();
        assert_eq!(
            (
                selected.profile_id.as_str(),
                selected.model.as_str(),
                selected.effort.as_deref()
            ),
            ("best", "fixed", Some("high"))
        );
        for (profile, model, effort) in [
            (Some("best"), None, None),
            (None, Some("fixed"), None),
            (None, None, Some("high")),
        ] {
            assert!(
                resolve_subagent_policy_selection(
                    &backend, "s", "parent", &policy, profile, model, effort
                )
                .await
                .is_err()
            );
        }
        for policy in [
            SubagentPolicy::None,
            SubagentPolicy::Native,
            SubagentPolicy::SingleModel {
                model: "missing".into(),
                effort: Some("high".into()),
            },
            SubagentPolicy::SingleModel {
                model: "fixed".into(),
                effort: Some("ultra".into()),
            },
        ] {
            assert!(
                resolve_subagent_policy_selection(
                    &backend, "s", "parent", &policy, None, None, None
                )
                .await
                .is_err()
            );
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

    // Hard-won: f67ed023: spawn accepted effort unsupported by the selected model
    #[tokio::test]
    async fn a_spawn_checks_effort_against_the_model_it_names() {
        let mut claude = candidate("claude", Some(50), &["sonnet", "no-effort"]);
        claude.choices.efforts = efforts(&["low", "medium", "high"]);
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            candidates: SubagentCandidates {
                offered: vec![claude],
                unavailable: Vec::new(),
            },
        });
        let refused = resolve_subagent_selection(
            &backend,
            "parent-session",
            "claude",
            None,
            Some("no-effort"),
            Some("low"),
        )
        .await
        .unwrap_err();
        assert!(
            refused.message.contains("offers no effort choices"),
            "{}",
            refused.message
        );
        let selection = resolve_subagent_selection(
            &backend,
            "parent-session",
            "claude",
            None,
            Some("no-effort"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(selection.effort, None);
        let selection = resolve_subagent_selection(
            &backend,
            "parent-session",
            "claude",
            None,
            Some("sonnet"),
            Some("low"),
        )
        .await
        .unwrap();
        assert_eq!(selection.effort.as_deref(), Some("low"));
    }
}
