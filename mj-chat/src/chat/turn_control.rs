//! Turn-control feedback follows durable execution, not submission receipts.
use super::*;
use crate::components::{ActionRole, Dialog};
use mj_core::relay::{RelayCommand, RelayOperationalState, SteeringStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Keep,
    Cancel,
    Retry,
    Remove,
}

pub(super) fn dialog() -> Dialog<Control> {
    let mut form = Dialog::new();
    for control in [
        Control::Keep,
        Control::Cancel,
        Control::Retry,
        Control::Remove,
    ] {
        form.declare(control, ControlKind::Button);
    }
    form.set_dismiss_actions(&[Control::Keep]);
    form.set_action_role(Control::Keep, ActionRole::Cancel);
    form.set_default_action(Control::Keep);
    form.end_frame(Control::Keep);
    form
}

impl ChatState {
    pub(super) fn escape_command(&self) -> Option<RelayCommand> {
        if !self.targeted_turn_control_supported {
            return Some(RelayCommand::CancelTurn);
        }
        let Some(active_prompt_id) = self.active_prompt_id.clone() else {
            return Some(RelayCommand::CancelTurn);
        };
        if let Some(queued) = self.queued_prompts.front().filter(|q| q.kind.is_prompt()) {
            Some(RelayCommand::Steer {
                active_prompt_id,
                queued_prompt_id: queued.id.clone(),
            })
        } else {
            Some(RelayCommand::CancelTurnFor { active_prompt_id })
        }
    }

    pub(super) fn sync_turn_control(&mut self, state: &RelayOperationalState) {
        self.targeted_turn_control_supported = state.supports_targeted_turn_control();
        let changed = self.steering != state.steering;
        if self.turn_control_awaiting_state.as_ref().is_some_and(|id| {
            state.steering.as_ref().is_some_and(|s| s.command_id == *id)
                || state
                    .cancelling_prompt_id
                    .as_ref()
                    .is_some_and(|target| Some(target) == self.turn_control_target.as_ref())
                || state.active_prompt.as_ref().map(|p| &p.command_id)
                    != self.turn_control_target.as_ref()
        }) {
            self.turn_control_awaiting_state = None;
        }
        self.active_prompt_id = state.active_prompt.as_ref().map(|p| p.command_id.clone());
        self.steering = state.steering.clone();
        self.cancelling_prompt_id = state.cancelling_prompt_id.clone();
        let feedback = if self.cancelling_prompt_id.is_some() {
            Some("Interrupting turn…")
        } else {
            self.steering.as_ref().and_then(|s| match s.status {
                SteeringStatus::Pending => Some("Steering…"),
                SteeringStatus::Unconfirmed => {
                    Some("Delivery unconfirmed · Esc reviews queued input")
                }
                _ => None,
            })
        };
        if let Some(text) = feedback {
            self.operation_feedback
                .insert("turn-control".into(), text.into());
        } else if !self.turn_control_submitting && self.turn_control_awaiting_state.is_none() {
            self.operation_feedback.remove("turn-control");
        }
        if changed {
            if let Some(s) = &self.steering {
                self.turn_control_error = s.message.clone();
                self.turn_control_target = Some(s.active_prompt_id.clone());
                self.turn_control_dialog_open = matches!(
                    s.status,
                    SteeringStatus::Failed | SteeringStatus::Unconfirmed
                );
                self.turn_control_dialog = dialog();
            } else {
                self.turn_control_dialog_open = false;
            }
        }
    }

    pub(super) fn handle_turn_control_dialog(&mut self, event: Event) -> ChatAction {
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Esc) {
            self.turn_control_dialog_open = false;
            return ChatAction::None;
        }
        let result = self.turn_control_dialog.handle(&event);
        let Some(Interaction::Activate(control)) = result.action else {
            return ChatAction::None;
        };
        let command = match control {
            Control::Keep => None,
            Control::Cancel => self
                .active_prompt_id
                .clone()
                .filter(|id| self.turn_control_target.as_ref() == Some(id))
                .map(|active_prompt_id| RelayCommand::CancelTurnFor { active_prompt_id }),
            Control::Retry if self.active_prompt_id.is_none() => {
                self.steering
                    .as_ref()
                    .map(|s| RelayCommand::ResolveSteering {
                        steering_id: s.command_id.clone(),
                    })
            }
            Control::Remove if self.active_prompt_id.is_none() => {
                self.steering
                    .as_ref()
                    .map(|s| RelayCommand::RemoveQueuedPrompt {
                        queued_command_id: s.queued_prompt_id.clone(),
                    })
            }
            _ => return ChatAction::None,
        };
        self.turn_control_dialog_open = false;
        command.map_or(ChatAction::None, ChatAction::TurnControl)
    }
}

pub(super) fn render(frame: &mut Frame, area: Rect, chat: &mut ChatState) {
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    let popup = crate::modal::centered_modal_fixed(frame, &mut chat.frame_surfaces, 72, 11, area);
    let block = Block::default().borders(Borders::ALL).title(" Steering ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let uncertain = chat
        .steering
        .as_ref()
        .is_some_and(|s| s.status == SteeringStatus::Unconfirmed);
    let mut text = chat
        .turn_control_error
        .clone()
        .unwrap_or_else(|| "The prompt could not be steered and remains queued.".into());
    if uncertain {
        text.push_str(
            " Delivery may already have occurred. Retrying can deliver the prompt twice.",
        );
    }
    let body = Rect {
        height: inner.height.saturating_sub(3),
        ..inner
    };
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), body);
    let buttons = if chat
        .active_prompt_id
        .as_ref()
        .is_some_and(|id| chat.turn_control_target.as_ref() == Some(id))
    {
        vec![
            (Control::Keep, "Keep queued", true),
            (
                Control::Cancel,
                if uncertain {
                    "Cancel turn"
                } else {
                    "Cancel turn and apply queued prompt"
                },
                chat.cancelling_prompt_id.is_none(),
            ),
        ]
    } else if uncertain {
        vec![
            (Control::Keep, "Keep queued", true),
            (Control::Retry, "Retry queued prompt", true),
            (Control::Remove, "Remove queued prompt", true),
        ]
    } else {
        vec![(Control::Keep, "Keep queued", true)]
    };
    chat.turn_control_dialog.begin_frame();
    Dialog::render_actions(
        frame,
        Rect {
            y: body.bottom(),
            height: inner.height.saturating_sub(body.height),
            ..inner
        },
        &buttons,
        &mut chat.turn_control_dialog,
    );
    chat.turn_control_dialog.end_frame(Control::Keep);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::{ActiveRelayPrompt, RelaySnapshot, SteeringOperation};

    #[test]
    fn reconnect_restores_pending_steering_and_escape_only_dismisses_failure_offer() {
        let mut chat = ChatState::from_materialized_tail(
            &mj_core::state::MaterializedSession::empty("owner"),
            &[],
            &[],
        );
        let mut state = RelaySnapshot::new("owner".into()).operational_state();
        state.active_prompt = Some(ActiveRelayPrompt {
            command_id: "active".into(),
            created_at_ms: 1,
            started_at_ms: 1,
        });
        state.steering = Some(SteeringOperation {
            command_id: "steer".into(),
            active_prompt_id: "active".into(),
            queued_prompt_id: "queued".into(),
            status: SteeringStatus::Pending,
            message: None,
        });
        chat.sync_turn_control(&state);
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(chat.handle_key(escape), ChatAction::None);
        assert_eq!(chat.operation_feedback["turn-control"], "Steering…");
        state.steering.as_mut().unwrap().status = SteeringStatus::Unconfirmed;
        chat.sync_turn_control(&state);
        assert!(chat.turn_control_dialog_open);
        assert_eq!(chat.handle_key(escape), ChatAction::None);
        assert!(!chat.turn_control_dialog_open);
        chat.sync_turn_control(&state);
        assert!(
            !chat.turn_control_dialog_open,
            "ordinary snapshots must not reopen a dismissed offer"
        );
        assert!(chat.operation_feedback["turn-control"].contains("unconfirmed"));
        state.steering.as_mut().unwrap().status = SteeringStatus::Applied;
        chat.sync_turn_control(&state);
        assert!(!chat.operation_feedback.contains_key("turn-control"));
    }

    #[test]
    fn explicit_cancellation_keeps_feedback_until_the_turn_settles() {
        let mut chat = ChatState::from_materialized_tail(
            &mj_core::state::MaterializedSession::empty("owner"),
            &[],
            &[],
        );
        let mut state = RelaySnapshot::new("owner".into()).operational_state();
        state.cancelling_prompt_id = Some("active".into());
        chat.sync_turn_control(&state);
        assert_eq!(
            chat.operation_feedback["turn-control"],
            "Interrupting turn…"
        );
        assert_eq!(
            chat.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ChatAction::None
        );
        state.cancelling_prompt_id = None;
        chat.sync_turn_control(&state);
        assert!(!chat.operation_feedback.contains_key("turn-control"));
    }
}
