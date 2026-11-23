//! `ask_oracle`: a primary's one read-only advisor. The first question starts
//! the oracle the way `spawn` starts a child, and later questions reach it the
//! way a parent's `send_message` reaches a child.

use super::*;
use mj_core::subagent::{SubagentKind, SubagentToolRequest};

const ACCEPTED_NEXT_ACTION: &str = "The oracle's answer arrives later as a Mjolnir message. Do independent work meanwhile; if none remains, end your turn with a brief waiting status.";

impl ApiBackend {
    pub(super) async fn ask_oracle(
        self: &Arc<Self>,
        primary_id: &str,
        request: &SubagentToolRequest,
        prepared: Option<&mj_controller::database::PreparedDelegation>,
        message: &str,
    ) -> Result<serde_json::Value> {
        ensure!(!message.trim().is_empty(), "the question cannot be empty");
        ensure!(
            mj_controller::database::load_subagent(primary_id)?.is_none(),
            "only top-level sessions may ask the oracle"
        );
        let question = format!("Question:\n{message}");
        // A replay of a start that already chose its model starts it again
        // unchanged, as `spawn` does.
        let (selected, reservation) = match prepared.and_then(|prepared| prepared.spawn.clone()) {
            Some(selected) => (selected, None),
            None => {
                let primary = self
                    .exports
                    .session_record(primary_id)
                    .context("primary session disappeared")?;
                let choices = primary
                    .effective_oracle(&self.exports.subagent_config())
                    .to_vec();
                ensure!(
                    !choices.is_empty(),
                    "The oracle is not configured for this session."
                );
                if let Some(oracle) = self.live_oracle(primary_id).await? {
                    return self.ask_live_oracle(&oracle, question, request).await;
                }
                let execution_policy = if self.exports.session_is_yolo_target(primary_id) {
                    mj_core::config::ExecutionPolicy::Unconstrained
                } else {
                    mj_core::config::ExecutionPolicy::ConfiguredApprovals
                };
                let backend: Arc<dyn crate::server::api::SubagentBackend> = self.clone();
                let selection = crate::server::api::resolve_oracle_selection(
                    &backend,
                    &primary.last_profile,
                    &choices,
                    execution_policy,
                )
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?;
                let reservation = Some(selection.reservation);
                let selected = mj_controller::database::PreparedSpawn {
                    profile_id: selection.profile_id,
                    model: selection.model,
                    effort: selection.effort,
                    fast_mode: selection.fast_mode,
                    prompt: question.clone(),
                };
                let selected = if prepared.is_some() {
                    blocking("persist selected oracle", {
                        let primary_id = primary_id.to_owned();
                        let request_id = request.request_id.clone();
                        move || {
                            mj_controller::database::prepare_delegation_spawn(
                                primary_id, request_id, selected,
                            )
                        }
                    })
                    .await?
                } else {
                    selected
                };
                (selected, reservation)
            }
        };
        let started = self
            .start_subagent(mj_controller::controller::RegisterSubagentRequest {
                no_github_credentials: false,
                kind: SubagentKind::Oracle,
                orchestrator: false,
                reviewer_launch: None,
                parent_session_id: primary_id.to_owned(),
                task_name: "oracle".into(),
                profile_id: selected.profile_id,
                model: Some(selected.model),
                effort: selected.effort,
                fast_mode: selected.fast_mode,
                working_directory: PathBuf::new(),
                initial_prompt: selected.prompt,
                request_key: request.request_id.clone(),
                report_root: None,
                category: "oracle".into(),
            })
            .await;
        match started {
            Ok(relation) => {
                if let Some(reservation) = reservation {
                    reservation.commit(Some(relation.child_session_id));
                }
            }
            // A concurrent first question registered the oracle first; this
            // one is an ordinary question to it.
            Err(error)
                if error
                    .downcast_ref::<mj_controller::database::OracleAlreadyLive>()
                    .is_some() =>
            {
                let oracle = self
                    .live_oracle(primary_id)
                    .await?
                    .context("the oracle stopped before it could be asked")?;
                return self.ask_live_oracle(&oracle, question, request).await;
            }
            Err(error) => return Err(error),
        }
        // An earlier oracle that cannot answer any more took its context with it.
        let earlier = blocking("list earlier oracles", {
            let primary_id = primary_id.to_owned();
            move || mj_controller::database::list_subagents(&primary_id)
        })
        .await?
        .iter()
        .any(|child| child.kind == SubagentKind::Oracle && child.request_key != request.request_id);
        let mut reply = serde_json::json!({
            "status": "accepted",
            "next_action": ACCEPTED_NEXT_ACTION,
        });
        if earlier {
            reply["context_lost"] = true.into();
            reply["notice"] = "The earlier oracle could no longer answer, so its context was lost; a fresh oracle is answering.".into();
        }
        Ok(reply)
    }

    /// The primary's newest oracle that can still be asked a question.
    async fn live_oracle(&self, primary_id: &str) -> Result<Option<String>> {
        let oracles = blocking("list oracles", {
            let primary_id = primary_id.to_owned();
            move || mj_controller::database::list_subagents(&primary_id)
        })
        .await?;
        Ok(oracles
            .into_iter()
            .rev()
            .filter(|child| child.kind == SubagentKind::Oracle)
            .find(|child| {
                self.exports
                    .session_record(&child.child_session_id)
                    .is_some_and(|record| record.state.can_receive_child_work())
            })
            .map(|child| child.child_session_id))
    }

    async fn ask_live_oracle(
        &self,
        oracle_id: &str,
        question: String,
        request: &SubagentToolRequest,
    ) -> Result<serde_json::Value> {
        self.deliver_to_own_child(
            oracle_id.to_owned(),
            question,
            request.request_id.clone(),
            request.created_at_ms,
        )
        .await?;
        Ok(serde_json::json!({
            "status": "accepted",
            "next_action": ACCEPTED_NEXT_ACTION,
        }))
    }
}
