//! Dashboard requests made on the person's behalf (`mj ui`).
//!
//! The daemon only checks a request and publishes it in the runtime feed;
//! each attached dashboard applies it as input. The dashboard stays the one
//! owner of its layout and of any Settings draft, and nothing here writes the
//! configuration: staged settings wait for the person to save them.
use super::*;
use mj_client::ui_request::{
    UiAction, UiRequest, UiRequestBody, UiRequestReceipt, UiRequestRefusal,
};

/// How many recent requests the feed carries. A dashboard applies only the
/// requests newer than the ones present when it attached, so this bounds
/// memory rather than delivery.
const RETAINED_UI_REQUESTS: usize = 16;

#[derive(Default)]
pub(super) struct UiRequestBoard {
    incarnation: String,
    next_id: u64,
    retained: VecDeque<UiRequest>,
}

impl UiRequestBoard {
    pub(super) fn published(&self) -> Vec<UiRequest> {
        self.retained.iter().cloned().collect()
    }
}

impl RuntimeState {
    /// Check a request against the current sessions and configuration, then
    /// publish it to every attached dashboard.
    pub(crate) fn request_ui(
        &self,
        body: UiRequestBody,
    ) -> std::result::Result<UiRequestReceipt, UiRequestRefusal> {
        match &body.action {
            UiAction::PaneSize { .. } => {}
            UiAction::ShowSession { session_id, .. } => {
                if self.session_record(session_id).is_none() {
                    return Err(UiRequestRefusal::Invalid(format!(
                        "there is no session {session_id}; list them with `mj sessions`"
                    )));
                }
            }
            UiAction::StageSettings { edits } => {
                // The same staging the dashboard will do, on the configuration
                // it holds now, so a mistake is reported to the agent here.
                let (_, mut draft) = self.with_config(mj_client::settings::draft::open);
                mj_client::settings::edit::stage_all(&mut draft, edits)
                    .map_err(UiRequestRefusal::Invalid)?;
            }
        }
        self.prune_dead_clients();
        let dashboards = self.attachments().len();
        if dashboards == 0 {
            return Err(UiRequestRefusal::NoDashboard);
        }
        let id = {
            let mut board = self
                .ui_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if board.incarnation.is_empty() {
                board.incarnation = new_command_id("ui-requests")
                    .map_err(|error| UiRequestRefusal::Invalid(format!("{error:#}")))?;
            }
            board.next_id += 1;
            let request = UiRequest {
                incarnation: board.incarnation.clone(),
                id: board.next_id,
                from_session_id: body.from_session_id,
                action: body.action,
            };
            board.retained.push_back(request);
            while board.retained.len() > RETAINED_UI_REQUESTS {
                board.retained.pop_front();
            }
            board.next_id
        };
        self.publish_revision();
        Ok(UiRequestReceipt { id, dashboards })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::ui_request::SettingEdit;

    fn stage(edits: &[(&str, serde_json::Value)]) -> UiRequestBody {
        UiRequestBody {
            from_session_id: None,
            action: UiAction::StageSettings {
                edits: edits
                    .iter()
                    .map(|(path, value)| SettingEdit {
                        path: path.split('.').map(str::to_owned).collect(),
                        value: value.clone(),
                    })
                    .collect(),
            },
        }
    }

    fn published(state: &RuntimeState) -> Vec<UiRequest> {
        state.runtime_publication().unwrap().metadata.ui_requests
    }

    // A request is all or nothing: one bad edit among good ones publishes
    // nothing, and nothing is published with no dashboard to apply it.
    #[tokio::test]
    async fn a_dashboard_request_is_published_only_whole_and_only_to_an_attached_dashboard() {
        let state = super::super::tests::test_runtime_state();
        let good = ("notify.bell", serde_json::json!(false));

        assert!(matches!(
            state.request_ui(stage(std::slice::from_ref(&good))),
            Err(UiRequestRefusal::NoDashboard)
        ));

        state.attachments().insert(
            "dashboard".into(),
            Attachment {
                renewed: std::time::Instant::now(),
            },
        );
        let Err(UiRequestRefusal::Invalid(reason)) = state.request_ui(stage(&[
            good.clone(),
            ("notify.mode", serde_json::json!("loud")),
        ])) else {
            panic!("an unknown notification mode was accepted");
        };
        assert!(reason.starts_with("notify.mode:"), "{reason}");
        assert!(published(&state).is_empty());

        let receipt = state.request_ui(stage(&[good])).unwrap();
        assert_eq!(receipt.dashboards, 1);
        let requests = published(&state);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, receipt.id);
        assert!(matches!(
            &requests[0].action,
            UiAction::StageSettings { edits } if edits[0].path == ["notify", "bell"]
        ));
    }
}
