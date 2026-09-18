use super::*;

impl ActiveChat {
    /// Applies one terminal event and reports what it asked for.
    pub fn handle_event(&mut self, event: Event) -> ChatEventOutcome {
        self.handle_event_result(event)
            .action
            .unwrap_or(ChatEventOutcome::None)
    }

    /// Applies one terminal event, reporting whether the conversation consumed
    /// it and whatever action it asks the host to take.
    pub fn handle_event_result(&mut self, event: Event) -> EventResult<ChatEventOutcome> {
        let action = match &event {
            Event::Key(key) => self.state.handle_key(*key),
            Event::Paste(pasted) => self.state.handle_terminal_paste(pasted),
            Event::Mouse(mouse) => self.state.handle_mouse(*mouse),
            // Resize and focus changes are handled by the host's geometry.
            _ => ChatAction::None,
        };
        let consumed = self.state.event_consumed(&event, &action);
        let dispatched = self.dispatch(action);
        dispatch_history_search_request(self.session.clone(), &mut self.state, &self.chat_io_tx);
        let action = (!matches!(dispatched, ChatEventOutcome::None)).then_some(dispatched);
        EventResult {
            consumed: consumed || action.is_some(),
            action,
        }
    }

    /// A command ID for one remote operation. `None` means the system random
    /// source failed, which leaves the command unsent rather than closing the
    /// view the user is reading.
    pub(crate) fn command_id(&mut self, prefix: &str) -> Option<String> {
        match new_command_id(prefix) {
            Ok(command_id) => Some(command_id),
            Err(error) => {
                self.state
                    .set_notice(format!("Could not identify the command: {error:#}"));
                None
            }
        }
    }

    pub(crate) fn dispatch(&mut self, action: ChatAction) -> ChatEventOutcome {
        match action {
            ChatAction::None => return ChatEventOutcome::None,
            ChatAction::OpenSubagents => return ChatEventOutcome::OpenSubagents,
            ChatAction::Prompt(text) => {
                let images = self.state.take_submitting_images();
                let Some(command_id) = self.command_id("prompt") else {
                    restore_unsent_prompt(&mut self.state, text, images);
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Prompt queued for delivery…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Prompt {
                        command_id,
                        text,
                        images,
                    },
                    &mut self.state,
                );
            }
            ChatAction::Attach { path, command } => {
                self.queue_attachment(AttachmentSource::Path(path), Some(command));
            }
            ChatAction::RunShell(command) => {
                let Some(command_id) = self.command_id("shell") else {
                    restore_unsent_input(&mut self.state, &format!("!{command}"));
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Shell command queued…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RunShell {
                        command_id,
                        command,
                    },
                    &mut self.state,
                );
            }
            ChatAction::RemoveQueuedPrompt { id, text, kind } => {
                let Some(command_id) = self.command_id("remove-prompt") else {
                    self.state.fail_queued_prompt_removal(id, text, kind);
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Removing queued prompt…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RemoveQueuedPrompt {
                        command_id,
                        id,
                        text,
                        kind,
                    },
                    &mut self.state,
                );
            }
            ChatAction::StopBackgroundTask { id } => {
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::StopBackgroundTask { id },
                    &mut self.state,
                );
            }
            ChatAction::GoalControl { action } => {
                let Some(command_id) = self.command_id("goal-control") else {
                    restore_unsent_input(&mut self.state, &format!("/goal {}", action.as_str()));
                    return ChatEventOutcome::Handled;
                };
                self.state
                    .set_notice(format!("Sending /goal {}…", action.as_str()));
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::GoalControl { command_id, action },
                    &mut self.state,
                );
            }
            ChatAction::SetConfig { key, value } => {
                let Some(command_id) = self.command_id("set-config") else {
                    restore_unsent_input(&mut self.state, &config_command_text(&key, &value));
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Sending configuration update…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::SetConfig {
                        command_id,
                        key,
                        value,
                    },
                    &mut self.state,
                );
            }
            ChatAction::PlanCommand {
                original,
                control,
                requested_active,
                prompt,
            } => {
                let Some(command_id) = self.command_id("plan-mode") else {
                    self.state.plan_command_pending = false;
                    self.state.finish_plan_mode_change(!requested_active);
                    restore_unsent_input(&mut self.state, &original);
                    return ChatEventOutcome::Handled;
                };
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::PlanCommand {
                        command_id,
                        original,
                        control,
                        requested_active,
                        prompt,
                    },
                    &mut self.state,
                );
            }
            ChatAction::Cancel => {
                let Some(command_id) = self.command_id("cancel") else {
                    return ChatEventOutcome::Handled;
                };
                let intent = self.state.turn_control_intent();
                self.state.set_notice(intent.sending_notice());
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Cancel {
                        command_id,
                        intent,
                        cancel_agent: self.state.prompt_in_flight()
                            || self.state.session_activity.capacity_retry.is_some(),
                        shell_command_ids: self.state.active_user_shell_ids(),
                    },
                    &mut self.state,
                );
            }
            ChatAction::StartSecondOpinion { request, proposal } => {
                self.open_second_opinion(request, proposal);
            }
            ChatAction::SecondOpinion(intent) => {
                self.run_second_opinion(intent);
            }
            ChatAction::StartTurnReview => self.request_turn_review(),
            ChatAction::TurnReview(intent) => self.run_turn_review(intent),
            ChatAction::RespondReviewerElicitation {
                role,
                elicitation_id,
                response,
            } => self.answer_reviewer(role, elicitation_id, response),
            ChatAction::RespondElicitation { request, response } => {
                let plan_followup = self.state.plan_review_followup(&request, &response);
                self.state.set_notice("Sending answer…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RespondElicitation {
                        request,
                        response,
                        plan_followup,
                    },
                    &mut self.state,
                );
            }
            ChatAction::PasteFromClipboard => {
                if self.paste_in_flight {
                    self.state.set_notice("Clipboard read already in progress…");
                    return ChatEventOutcome::Handled;
                }
                self.paste_in_flight = true;
                self.state.set_notice("Reading clipboard…");
                let updates = self.chat_io_tx.clone();
                let text_only = self.state.clipboard_is_text_only();
                let generation = self.state.input_generation();
                tokio::spawn(async move {
                    let result = match tokio::task::spawn_blocking(move || {
                        if text_only {
                            crate::clipboard::read_text()
                                .map(ClipboardContent::Text)
                                .map_err(|error| format!("{error:#}"))
                        } else {
                            crate::clipboard::read().map_err(|error| format!("{error:#}"))
                        }
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(format!("clipboard task failed: {error}")),
                    };
                    if let Err(error) = updates.send(ChatIoUpdate::Clipboard { generation, result })
                    {
                        tracing::debug!(%error, "clipboard result dropped because the chat closed");
                    }
                });
            }
            ChatAction::ToggleVoice => {
                if let Some(cancel) = self.voice_cancel.as_ref() {
                    let (command, notice) = if self.voice_finishing {
                        (crate::speech::VoiceCommand::Cancel, "Cancelling dictation…")
                    } else {
                        self.voice_finishing = true;
                        (
                            crate::speech::VoiceCommand::Finish,
                            "Finishing dictation… click the microphone again to cancel",
                        )
                    };
                    if let Err(error) = cancel.send(command) {
                        self.state
                            .set_notice(format!("Dictation worker stopped: {error}"));
                    } else {
                        self.state.set_notice(notice);
                    }
                } else if let Some(auth_path) = self.voice_auth.clone() {
                    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
                    self.voice_cancel = Some(cancel_tx);
                    self.voice_finishing = false;
                    self.state.voice_active = true;
                    self.state
                        .set_notice("Starting microphone… click it again to transcribe");
                    spawn_dictation(auth_path, self.voice_updates_tx.clone(), cancel_rx);
                }
            }
            // Moving the keyboard to another pane is not leaving the
            // conversation: it stays on screen, so nothing is detached and
            // dictation keeps running.
            ChatAction::CycleFocus { reverse } => {
                return ChatEventOutcome::CycleFocus { reverse };
            }
            ChatAction::QuitDetach => return self.detach(),
        }
        ChatEventOutcome::Handled
    }

    /// Switches the transcript between rendered Markdown and raw source.
    ///
    /// The key that runs this belongs to the host's command registry, so the
    /// host calls it directly rather than through a composer key.
    pub fn toggle_transcript_rendering(&mut self) {
        self.state.toggle_render_mode();
    }

    /// Starts, finishes or cancels dictation, exactly as clicking the
    /// microphone does. Nothing happens while dictation is unavailable and no
    /// recording is running.
    pub fn toggle_dictation(&mut self) {
        let action = self.state.dictation_toggle_action();
        if action == ChatAction::None {
            return;
        }
        // Dictation never asks the host to leave the conversation, so there
        // is no outcome to forward.
        let _ = self.dispatch(action);
    }

    /// Leaves the conversation: stops any dictation and reports how far the
    /// transcript has been read, which the host turns into the session's read
    /// receipt and its saved draft.
    ///
    /// The detach key is a host binding, so the host catches it before the
    /// composer sees the key and calls this directly; `/detach` reaches it
    /// through [`ChatAction::QuitDetach`]. Both paths must do the same
    /// bookkeeping.
    pub fn detach(&mut self) -> ChatEventOutcome {
        self.cancel_dictation();
        ChatEventOutcome::QuitDetach {
            last_seen_event_ordinal: detach_chat(&mut self.state),
        }
    }
}
