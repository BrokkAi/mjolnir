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
    if !matches!(parent.harness_kind.as_str(), "claude" | "codex") {
        return Err(ApiFailure::conflict(
            "only Claude and Codex sessions can spawn sub-agents",
        ));
    }
    validate_prompt_text(&request.instructions, false)?;
    if request.task_name.trim().is_empty() {
        return Err(ApiFailure::bad_request("task_name cannot be empty"));
    }
    if request.request_key.trim().is_empty() {
        return Err(ApiFailure::bad_request("request_key cannot be empty"));
    }
    let profile_id = request
        .profile_id
        .clone()
        .unwrap_or_else(|| parent.profile_id.clone());
    let mut selected_model = request.model.clone();
    let mut selected_effort = request.effort.clone();
    if profile_id == parent.profile_id
        && (selected_model.is_none() || selected_effort.is_none())
        && let Some(handle) = backend.session_handle(parent_session_id.clone()).await?
        && let Some(snapshot) = handle.view().snapshot
    {
        selected_model =
            selected_model.or_else(|| snapshot.operational.config.get("model").cloned());
        selected_effort =
            selected_effort.or_else(|| snapshot.operational.config.get("effort").cloned());
    }
    // Checked against the warm catalogue only. Discovering a profile launches
    // a harness, which takes tens of seconds, and the caller is a model
    // waiting on its tool call. A selector the catalogue could not check is
    // validated by the start follow-up against the child's live harness; an
    // unsupported one fails the child's start and is reported to the parent as
    // that child's error through `wait` and `list_agents`.
    if (selected_model.is_some() || selected_effort.is_some())
        && let Some(choices) = backend.published_profile_config(&profile_id)
    {
        validate_selectors(
            &choices,
            selected_model.as_deref(),
            selected_effort.as_deref(),
        )?;
    }

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
            profile_id,
            model: selected_model.clone(),
            effort: selected_effort.clone(),
            working_directory: request.working_directory.unwrap_or_default(),
            initial_prompt: initial_prompt.clone(),
            request_key: request.request_key,
        })
        .await
        .map_err(|error| ApiFailure::conflict(format!("sub-agent creation failed: {error:#}")))?;
    backend
        .start_followup(
            relation.child_session_id.clone(),
            StartFollowup {
                model: selected_model,
                effort: selected_effort,
                prompt: Some(initial_prompt),
            },
        )
        .await?;
    let session = {
        let snapshot = state.snapshot_rx.borrow();
        ApiSession::from(require_session_record(
            &snapshot,
            &relation.child_session_id,
        )?)
    };
    Ok((
        StatusCode::CREATED,
        Json(SubagentView {
            parent_session_id,
            task_name: relation.task_name,
            request_key: relation.request_key,
            session,
        }),
    ))
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
                request_key: record.request_key,
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
