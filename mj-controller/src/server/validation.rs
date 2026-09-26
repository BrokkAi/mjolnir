use super::*;

/// Check attached images without decoding megabytes of base64 on the task that
/// serves the request. Everything else about an action is cheap enough to
/// check inline; a full multi-image prompt is not.
pub(super) async fn decode_prompt_images_off_task(
    action: ControllerAction,
) -> Result<ControllerAction, ApiError> {
    if let ControllerAction::Prompt {
        command_id: Some(id),
        ..
    }
    | ControllerAction::RunShell {
        command_id: Some(id),
        ..
    } = &action
    {
        validate_public_id(id)?;
    }
    let ControllerAction::Prompt { images, .. } = &action else {
        return Ok(action);
    };
    if images.is_empty() {
        return Ok(action);
    }
    tokio::task::spawn_blocking(move || {
        let mut action = action;
        let ControllerAction::Prompt {
            session_id, images, ..
        } = &action
        else {
            unreachable!("only prompt actions carry images")
        };
        validate_prompt_images(images)?;
        let store = AttachmentStore::controller(session_id).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not open the image attachment store",
            )
        })?;
        let ControllerAction::Prompt { images, .. } = &mut action else {
            unreachable!("only prompt actions carry images")
        };
        for image in images {
            if let Some(reference) = image.attachment.clone() {
                // Reading through this session's store both verifies the
                // digest and prevents a reference from another session being
                // smuggled into a prompt.
                store
                    .read(&reference)
                    .map_err(|_| ApiError::bad_request("the image attachment is unavailable"))?;
                image.data_base64.clear();
                image.mime_type = reference.mime_type;
                image.width = reference.width;
                image.height = reference.height;
            } else {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data_base64)
                    .map_err(|_| ApiError::bad_request("image data must be valid base64"))?;
                let optimized = optimize_image(&bytes).map_err(|_| {
                    ApiError::bad_request("unsupported image format or image could not be decoded")
                })?;
                let reference = AttachmentRef::new(
                    &optimized.bytes,
                    optimized.mime_type.clone(),
                    optimized.width,
                    optimized.height,
                )
                .map_err(|_| {
                    ApiError::bad_request("the inline image could not become an attachment")
                })?;
                store.install(&reference, &optimized.bytes).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not store the image attachment",
                    )
                })?;
                image.data_base64.clear();
                image.attachment = Some(reference);
                image.mime_type = optimized.mime_type;
                image.width = optimized.width;
                image.height = optimized.height;
            }
        }
        Ok(action)
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the server could not check the attached images",
        )
    })?
}

pub(super) fn validate_prompt_images(images: &[ViewerPromptImage]) -> Result<(), ApiError> {
    if images.len() > MAX_PROMPT_IMAGES {
        return Err(ApiError::bad_request(
            "a prompt may contain at most 10 images",
        ));
    }
    for image in images {
        if !image.mime_type.starts_with("image/") {
            return Err(ApiError::bad_request(
                "image mime type must start with image/",
            ));
        }
        if image.width == 0 || image.height == 0 {
            return Err(ApiError::bad_request(
                "image dimensions must be greater than zero",
            ));
        }
        if let Some(reference) = &image.attachment {
            if !image.data_base64.is_empty() {
                return Err(ApiError::bad_request(
                    "an image cannot contain both inline data and an attachment",
                ));
            }
            if reference.mime_type != image.mime_type
                || reference.width != image.width
                || reference.height != image.height
            {
                return Err(ApiError::bad_request(
                    "image attachment metadata does not match the prompt",
                ));
            }
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| ApiError::bad_request("image data must be valid base64"))?;
        if bytes.is_empty() {
            return Err(ApiError::bad_request("image data must not be empty"));
        }
    }
    Ok(())
}

pub(super) const MAX_MOVE_QUEUE_ITEMS: usize = 256;
pub(super) const MAX_MOVE_MOUNTS: usize = 32;

pub(super) fn validate_move_selection(
    selection: &MoveSelection,
    snapshot: &ViewerSnapshot,
) -> Result<(), ApiError> {
    validate_public_id(&selection.session_id)?;
    if selection.profile_id.is_none() && selection.target_template_id.is_none() {
        return Err(ApiError::bad_request(
            "move must select a profile, a target, or both",
        ));
    }
    if selection.clear_resource_allocation && selection.resource_allocation.is_some() {
        return Err(ApiError::bad_request(
            "clear resource sizing cannot be combined with an explicit allocation",
        ));
    }
    if let Some(profile_id) = selection.profile_id.as_deref() {
        validate_public_id(profile_id)?;
        require_profile(snapshot, profile_id)?;
    }
    if let Some(target_id) = selection.target_template_id.as_deref() {
        validate_public_id(target_id)?;
        require_target(snapshot, target_id)?;
        let session = require_session_record(snapshot, &selection.session_id)?;
        if session
            .incompatible_resume_targets
            .iter()
            .any(|id| id == target_id)
        {
            return Err(ApiError::bad_request(
                "this session cannot resume on that target",
            ));
        }
    } else {
        require_session_record(snapshot, &selection.session_id)?;
    }
    if let Some(mounts) = &selection.additional_mounts {
        validate_move_mounts(mounts)?;
    }
    let session = require_session_record(snapshot, &selection.session_id)?;
    let retryable_move = session
        .move_recovery
        .as_ref()
        .is_some_and(|recovery| recovery.checkpoint_retained);
    if !session.capabilities.move_session && !retryable_move {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this session cannot be moved now",
        ));
    }
    Ok(())
}

pub(super) fn validate_move_mounts(mounts: &[AdditionalMount]) -> Result<(), ApiError> {
    if mounts.len() > MAX_MOVE_MOUNTS {
        return Err(ApiError::bad_request("a move may carry at most 32 mounts"));
    }
    for mount in mounts {
        for path in [&mount.source, &mount.destination] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| component == Component::ParentDir)
            {
                return Err(ApiError::bad_request(
                    "move mount paths must be absolute and must not contain '..'",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_resume_settings(
    additional_mounts: Option<&Vec<AdditionalMount>>,
    resource_allocation: Option<&SessionResourceAllocation>,
) -> Result<(), ApiError> {
    if let Some(mounts) = additional_mounts {
        validate_move_mounts(mounts)?;
    }
    if let Some(allocation) = resource_allocation {
        allocation
            .validate()
            .map_err(|_| ApiError::bad_request("resource allocation is invalid"))?;
    }
    Ok(())
}

pub(super) fn validate_move_request(
    request: &MoveSessionRequest,
    snapshot: &ViewerSnapshot,
) -> Result<(), ApiError> {
    let preparation = &request.preparation;
    validate_move_selection(&preparation.selection, snapshot)?;
    let session = require_session_record(snapshot, &preparation.selection.session_id)?;
    if preparation.operation_id.trim().is_empty() || preparation.fingerprint.trim().is_empty() {
        return Err(ApiError::bad_request(
            "move confirmation is missing its preparation identity",
        ));
    }
    if preparation.queued_commands.len() > MAX_MOVE_QUEUE_ITEMS {
        return Err(ApiError::bad_request(
            "move queue is too large; prepare again",
        ));
    }
    let active_now = preparation.active
        || session.chat_phase == ViewerChatPhase::Running
        || !session.active_user_shells.is_empty();
    if active_now && !request.acknowledge_interruption {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "confirm that the active turn may be interrupted",
        ));
    }
    if !preparation.queued_commands.is_empty() && request.queue.is_none() {
        return Err(ApiError::bad_request(
            "choose whether queued work is discarded or started after the move",
        ));
    }
    Ok(())
}

/// What every surface accepts as prompt text.
///
/// Session creation carries a first prompt before any session record exists,
/// so this is separate from [`validate_action`]'s prompt arm rather than being
/// restated there: both must refuse the same text.
pub(super) fn validate_prompt_text(text: &str, has_images: bool) -> Result<(), ApiError> {
    if text.starts_with('!') {
        return Err(ApiError::bad_request(
            "leading ! is reserved for shell commands",
        ));
    }
    if text.chars().count() > MAX_PROMPT_CHARS {
        return Err(ApiError::bad_request(
            "prompt must contain 1-65536 characters",
        ));
    }
    if text.trim().is_empty() && !has_images {
        return Err(ApiError::bad_request(
            "prompt must contain text or an image",
        ));
    }
    Ok(())
}

pub(super) fn validate_action(
    action: &ControllerAction,
    snapshot: &ViewerSnapshot,
) -> Result<(), ApiError> {
    validate_action_against(action, snapshot, None)
}

/// The configuration options the running session reports right now, or `None`
/// when no live session actor can answer.
///
/// The controller snapshot is republished asynchronously, so it still lists the
/// previous model's choices for a few seconds after a model change. Validating
/// a dependent selector against it refuses an effort the new model does offer.
pub(super) async fn live_session_config_options(
    state: &ServerState,
    session_id: &str,
    harness: mj_core::config::HarnessKind,
) -> Option<Vec<ViewerConfigOption>> {
    let backend = state.subagent.as_ref()?;
    let handle = backend.session_handle(session_id.to_owned()).await.ok()??;
    let snapshot = handle.view().snapshot?;
    let options = session_config_view(harness, &snapshot.operational);
    // An empty list is the absence of an answer, not an agent that offers
    // nothing, so it must not become the authority the request is refused by.
    (!options.is_empty()).then_some(options)
}

/// Validate an action against the live session where that matters.
///
/// Only `SetConfig` depends on choices the session can change mid-flight, so
/// only it pays for the extra lookup; everything else validates against the
/// snapshot exactly as before.
pub(super) async fn validate_action_live(
    state: &ServerState,
    action: &ControllerAction,
) -> Result<(), ApiError> {
    let live = match action {
        ControllerAction::SetConfig { session_id, .. } => {
            let harness = {
                let snapshot = state.snapshot_rx.borrow();
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == *session_id)
                    .and_then(|session| {
                        session
                            .harness_kind
                            .parse::<mj_core::config::HarnessKind>()
                            .ok()
                    })
            };
            match harness {
                Some(harness) => live_session_config_options(state, session_id, harness).await,
                None => None,
            }
        }
        _ => None,
    };
    validate_action_against(action, &state.snapshot_rx.borrow(), live.as_deref())
}

fn validate_action_against(
    action: &ControllerAction,
    snapshot: &ViewerSnapshot,
    live_config_options: Option<&[ViewerConfigOption]>,
) -> Result<(), ApiError> {
    match action {
        ControllerAction::New {
            workspace_id,
            profile_id,
            bundle_id,
            target_id,
            title,
            project_directory,
            dirty_ack,
            create_managed_worktree,
            launch_base: _,
            launch_branch: _,
            checkout: _,
            mjolnir_subagents: _,
        } => {
            if !workspace_id.is_empty() {
                validate_public_id(workspace_id)?;
            }
            validate_public_id(profile_id)?;
            validate_public_id(target_id)?;
            if let Some(title) = title {
                validate_title(title)?;
            }
            // An acknowledgement names repositories the preflight reported.
            // Unbounded or malformed entries would travel to the controller
            // and be compared against a real set, so they are refused here.
            if dirty_ack.len() > MAX_DIRTY_ACKNOWLEDGEMENTS
                || dirty_ack
                    .iter()
                    .any(|repository| repository.trim().is_empty() || repository.len() > 256)
            {
                return Err(ApiError::bad_request(
                    "dirty acknowledgement must name 0-32 repositories",
                ));
            }
            require_profile(snapshot, profile_id)?;
            let target = require_target(snapshot, target_id)?;
            // Bare sessions open the selected directory; the browser has no
            // bundle selection to supply for that flow.
            if !target.requires_project_directory || !bundle_id.is_empty() {
                validate_public_id(bundle_id)?;
                require_bundle(snapshot, bundle_id)?;
            }
            if *create_managed_worktree == Some(true) && !target.requires_project_directory {
                return Err(ApiError::bad_request(
                    "managed worktree creation requires a bare Git project",
                ));
            }
            if target.requires_project_directory != project_directory.is_some() {
                return Err(ApiError::bad_request(
                    "project_directory is required exactly for bare targets",
                ));
            }
            if let Some(directory) = project_directory
                && (mj_core::path_input::validate_absolute_input(directory).is_err()
                    || directory
                        .components()
                        .any(|component| component == Component::ParentDir))
            {
                return Err(ApiError::bad_request(
                    "project_directory must be an absolute safe path",
                ));
            }
        }
        ControllerAction::Resume {
            session_id,
            workspace_id,
            profile_id,
            target_id,
            additional_mounts,
            resource_allocation,
            ..
        } => {
            validate_public_id(session_id)?;
            validate_public_id(workspace_id)?;
            validate_public_id(profile_id)?;
            validate_public_id(target_id)?;
            let session = require_session_record(snapshot, session_id)?;
            require_workspace(snapshot, workspace_id)?;
            require_profile(snapshot, profile_id)?;
            require_target(snapshot, target_id)?;
            if session
                .incompatible_resume_targets
                .iter()
                .any(|incompatible| incompatible == target_id)
            {
                return Err(ApiError::bad_request(
                    "this session cannot resume on that target",
                ));
            }
            validate_resume_settings(additional_mounts.as_ref(), resource_allocation.as_ref())?;
        }
        ControllerAction::Move { request } => validate_move_request(request, snapshot)?,
        ControllerAction::Open { session_id }
        | ControllerAction::Suspend { session_id, .. }
        | ControllerAction::Destroy { session_id, .. }
        | ControllerAction::Cancel { session_id }
        | ControllerAction::StartReview { session_id } => {
            validate_public_id(session_id)?;
            require_session_record(snapshot, session_id)?;
        }
        ControllerAction::ResolveReview {
            session_id,
            resolution,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            let Some(resolution) = resolution_from_name(resolution) else {
                return Err(ApiError::bad_request(
                    "a review is resolved by forward, dismiss, or cancel",
                ));
            };
            let Some(review) = session.turn_review.as_ref() else {
                return Err(ApiError::bad_request("no review is open for that session"));
            };
            // Cancel is always available; the rest wait for the verdict the
            // daemon published, which is the same gate the daemon enforces
            // when it actually resolves.
            let allowed = resolution == mj_core::review::driver::Resolution::Cancelled
                || review.verdict.as_ref().is_some_and(|verdict| {
                    resolution_name(&resolution)
                        .is_some_and(|name| verdict.allowed.iter().any(|allowed| allowed == name))
                });
            if !allowed {
                return Err(ApiError::bad_request(
                    "that review cannot be resolved that way yet",
                ));
            }
        }
        ControllerAction::Rename { session_id, title } => {
            validate_public_id(session_id)?;
            validate_title(title)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.rename {
                return Err(ApiError::bad_request("this session cannot be renamed"));
            }
        }
        ControllerAction::TurnControl {
            session_id,
            command,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            let allowed = match command {
                mj_core::relay::RelayCommand::CancelTurnFor { .. } => {
                    session.capabilities.interrupt_turn
                }
                mj_core::relay::RelayCommand::Steer { .. }
                | mj_core::relay::RelayCommand::ResolveSteering { .. } => {
                    session.capabilities.prompt
                }
                _ => false,
            };
            if !allowed {
                return Err(ApiError::bad_request(
                    "this turn-control action is unavailable",
                ));
            }
        }
        ControllerAction::InterruptTurn { session_id } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.interrupt_turn {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this session has no turn to cancel",
                ));
            }
        }
        ControllerAction::SetConfig {
            session_id,
            key,
            value,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.set_config {
                return Err(ApiError::bad_request(
                    "this session cannot change configuration now",
                ));
            }
            // The harness decides what it accepts. Forwarding a key it never
            // advertised, or a value outside the ones it offered, asks it to
            // refuse something the viewer should not have offered.
            //
            // The live session's own list wins when there is one: a model
            // change replaces the effort catalogue immediately, while the
            // snapshot still carries the previous model's choices.
            let option = live_config_options
                .unwrap_or(&session.config_options)
                .iter()
                .find(|option| option.key == *key)
                .ok_or_else(|| ApiError::bad_request("this agent does not offer that setting"))?;
            if !option.choices.iter().any(|choice| choice.value == *value) {
                return Err(ApiError::bad_request(
                    "this agent does not offer that value for that setting",
                ));
            }
        }
        ControllerAction::SetPlanMode { session_id, .. } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.set_plan_mode {
                return Err(ApiError::bad_request(
                    "this session cannot change plan mode now",
                ));
            }
        }
        ControllerAction::RefreshQuota { profile_id } => {
            validate_public_id(profile_id)?;
            require_profile(snapshot, profile_id)?;
        }
        ControllerAction::RefreshCapacity { target_id } => {
            validate_public_id(target_id)?;
            require_target(snapshot, target_id)?;
        }
        ControllerAction::Prompt {
            session_id,
            text,
            images,
            ..
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if images.len() > MAX_PROMPT_IMAGES {
                return Err(ApiError::bad_request(
                    "a prompt may contain at most 10 images",
                ));
            }
            validate_prompt_text(text, !images.is_empty())?;
            if !images.is_empty() && !session.prompt_images_supported {
                return Err(ApiError::bad_request(
                    "this session does not support image prompts",
                ));
            }
            // Review is synchronous: the turn under review stays where the
            // review found it. The daemon's own submit path is what makes this
            // true; refusing here as well is what turns it into an immediate
            // answer rather than a rejected prompt.
            if session.turn_review.is_some() {
                return Err(ApiError::bad_request(
                    crate::review_host::PROMPT_HELD_MESSAGE,
                ));
            }
        }
        ControllerAction::RunShell {
            session_id,
            command,
            ..
        } => {
            validate_public_id(session_id)?;
            require_session_record(snapshot, session_id)?;
            if command.trim().is_empty() || command.chars().count() > MAX_PROMPT_CHARS {
                return Err(ApiError::bad_request(
                    "shell command must contain 1-65536 characters",
                ));
            }
        }
        ControllerAction::CancelShell {
            session_id,
            shell_command_id,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(shell_command_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session
                .active_user_shells
                .iter()
                .any(|shell| shell.id == *shell_command_id)
            {
                return Err(ApiError::bad_request("unknown active shell command"));
            }
        }
        ControllerAction::RemoveQueuedPrompt {
            session_id,
            queue_id,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(queue_id)?;
            require_session_record(snapshot, session_id)?;
        }
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(elicitation_id)?;
            let session = require_session_record(snapshot, session_id)?;
            let request = session
                .pending_elicitations
                .iter()
                .find(|request| request.id == *elicitation_id)
                .ok_or_else(|| ApiError::not_found("unknown elicitation"))?;
            if serde_json::to_vec(response).map_or(usize::MAX, |encoded| encoded.len())
                > MAX_ELICITATION_BYTES
            {
                return Err(ApiError::bad_request("elicitation answer is too large"));
            }
            // The answer has to satisfy the question the agent actually asked.
            // A phone can post one for a request the session has already
            // replaced, and forwarding that would answer a live question with
            // content the agent never offered.
            if request.validate_response(response).is_err() {
                return Err(ApiError::bad_request(
                    "the answer does not match this elicitation request",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_public_id(id: &str) -> Result<(), ApiError> {
    validate_id("request", id).map_err(|_| ApiError::bad_request("invalid id"))
}

pub(super) fn validate_title(title: &str) -> Result<(), ApiError> {
    if title.trim().is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        Err(ApiError::bad_request("title must contain 1-120 characters"))
    } else {
        Ok(())
    }
}

pub(super) fn require_session_record<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerSession, ApiError> {
    snapshot
        .sessions
        .iter()
        .find(|session| session.id == id)
        .ok_or_else(|| ApiError::not_found("unknown session"))
}

pub(super) fn require_workspace(snapshot: &ViewerSnapshot, id: &str) -> Result<(), ApiError> {
    snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.id == id)
        .then_some(())
        .ok_or_else(|| ApiError::bad_request("unknown workspace"))
}

pub(super) fn require_profile<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerProfile, ApiError> {
    snapshot
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .ok_or_else(|| ApiError::bad_request("unknown profile"))
}

pub(super) fn require_target<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerTarget, ApiError> {
    snapshot
        .targets
        .iter()
        .find(|target| target.id == id)
        .ok_or_else(|| ApiError::bad_request("unknown target"))
}

pub(super) fn require_bundle(snapshot: &ViewerSnapshot, id: &str) -> Result<(), ApiError> {
    snapshot
        .bundles
        .iter()
        .any(|bundle| bundle.id == id)
        .then_some(())
        .ok_or_else(|| ApiError::bad_request("unknown bundle"))
}
