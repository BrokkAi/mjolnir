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
    validate_prompt_text(&request.instructions, false)?;
    if request.task_name.trim().is_empty() {
        return Err(ApiFailure::bad_request("task_name cannot be empty"));
    }
    let selection = resolve_subagent_selection(
        &backend,
        &parent_session_id,
        &parent.profile_id,
        request.profile_id.as_deref(),
        request.model.as_deref(),
        request.effort.as_deref(),
    )
    .await?;

    let initial_prompt = build_subagent_prompt(
        &backend,
        &parent_session_id,
        &request.instructions,
        request.context.as_deref(),
        &request.files,
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
    let candidates = backend
        .subagent_candidates(parent_profile.to_owned())
        .await?;
    let chosen = choose_subagent_profile(candidates, profile_id, parent_profile, &model)?;
    let effort = match effort {
        Some(effort) => {
            validate_selectors(&chosen.choices, None, Some(effort))?;
            Some(effort.to_owned())
        }
        None => parent_config.get("effort").cloned().filter(|effort| {
            chosen
                .choices
                .efforts
                .iter()
                .any(|choice| &choice.value == effort)
        }),
    };
    let fast_mode = mj_core::codex_catalog::is_luna_model(&model);
    Ok(SubagentSelection {
        profile_id: chosen.profile_id,
        model,
        effort,
        fast_mode,
    })
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

pub(crate) async fn build_subagent_prompt(
    backend: &Arc<dyn SubagentBackend>,
    parent_session_id: &str,
    instructions: &str,
    context: Option<&str>,
    ranges: &[SubagentSourceRange],
) -> Result<String, ApiFailure> {
    let mut prompt = String::new();
    prompt.push_str(instructions.trim());
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
    fn a_model_no_profile_offers_is_refused_with_what_is_offered() {
        let failure = choose_subagent_profile(
            SubagentCandidates {
                offered: vec![
                    candidate("codex", Some(50), &["luna", "nova"]),
                    candidate("deepseek", Some(100), &["flash"]),
                ],
                unavailable: vec![("glm".to_owned(), "not signed in".to_owned())],
            },
            None,
            "codex",
            "sol",
        )
        .unwrap_err();
        assert_eq!(failure.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            failure.message,
            "no eligible profile offers model \"sol\". Offered: deepseek (flash); \
             codex (luna, nova). Could not check: glm (not signed in)."
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
    async fn a_luna_model_selection_turns_on_fast_mode() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            candidates: SubagentCandidates {
                offered: vec![candidate("codex", Some(50), &["luna"])],
                unavailable: Vec::new(),
            },
        });
        let selection = resolve_subagent_selection(
            &backend,
            "parent-session",
            "codex",
            None,
            Some("luna"),
            None,
        )
        .await
        .unwrap();
        assert!(selection.fast_mode);
    }

    #[tokio::test]
    async fn a_non_luna_model_selection_leaves_fast_mode_off() {
        let backend: Arc<dyn SubagentBackend> = Arc::new(FakeSelectionBackend {
            candidates: SubagentCandidates {
                offered: vec![candidate("codex", Some(50), &["nova", "astra"])],
                unavailable: Vec::new(),
            },
        });
        for model in ["nova", "astra"] {
            let selection = resolve_subagent_selection(
                &backend,
                "parent-session",
                "codex",
                None,
                Some(model),
                None,
            )
            .await
            .unwrap();
            assert!(!selection.fast_mode, "{model} must not turn on fast mode");
        }
    }
}
