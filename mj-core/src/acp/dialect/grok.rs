//! Grok Build's legacy ACP extensions.

use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelect, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionConfigValueId, SessionId,
};
use agent_client_protocol::{Agent, ConnectionTo};
use anyhow::{Context, Result, bail, ensure};

use crate::config::HarnessKind;
use crate::elicitation::ElicitationResponse;

use super::super::plan_review_answer;

const EXIT_PLAN_MODE_METHOD: &str = "x.ai/exit_plan_mode";
const SET_MODEL_METHOD: &str = "session/set_model";
const PLAN_REVIEW_ID_PREFIX: &str = "plan-review-grok-";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrokModelState {
    current_model_id: String,
    current_effort: Option<String>,
    models: Vec<GrokModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrokModel {
    id: String,
    name: String,
    description: Option<String>,
    default_effort: Option<String>,
    efforts: Vec<GrokChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokChoice {
    pub id: String,
    name: String,
    description: Option<String>,
}

impl GrokModelState {
    fn current_model(&self) -> Option<&GrokModel> {
        self.models
            .iter()
            .find(|model| model.id == self.current_model_id)
    }
}

fn is_exit_plan_mode_method(method: &str) -> bool {
    method.strip_prefix('_').unwrap_or(method) == EXIT_PLAN_MODE_METHOD
}

pub fn handles_exit_plan_mode(harness: HarnessKind, method: &str) -> bool {
    harness == HarnessKind::Grok && is_exit_plan_mode_method(method)
}

pub fn plan_review_id(sequence: u64) -> String {
    format!("{PLAN_REVIEW_ID_PREFIX}{sequence}")
}

pub fn is_plan_review_id(id: &str) -> bool {
    id.starts_with(PLAN_REVIEW_ID_PREFIX)
}

pub fn plan_response(response: ElicitationResponse) -> serde_json::Value {
    let (action, feedback) = plan_review_answer(response);
    match action.as_str() {
        "implement" => serde_json::json!({ "outcome": "approved" }),
        "exit" => serde_json::json!({ "outcome": "abandoned" }),
        "revise" => feedback.map_or_else(
            || serde_json::json!({ "outcome": "cancelled" }),
            |feedback| serde_json::json!({ "outcome": "cancelled", "feedback": feedback }),
        ),
        // Keep planning, and a second opinion if one ever reached here rather
        // than being answered locally: neither approves the plan.
        _ => serde_json::json!({ "outcome": "cancelled" }),
    }
}

/// Read Grok's model catalogue from ACP initialization or session metadata.
pub fn model_state(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<GrokModelState> {
    model_state_from_value(meta?.get("modelState")?)
}

fn model_state_from_value(state: &serde_json::Value) -> Option<GrokModelState> {
    let current_model_id = state.get("currentModelId")?.as_str()?.to_owned();
    let models = state
        .get("availableModels")?
        .as_array()?
        .iter()
        .filter_map(|model| {
            let id = model.get("modelId")?.as_str()?.to_owned();
            let efforts = model
                .pointer("/_meta/reasoningEfforts")
                .and_then(serde_json::Value::as_array)
                .map(|efforts| {
                    efforts
                        .iter()
                        .filter_map(|effort| {
                            let id = effort
                                .get("value")
                                .or_else(|| effort.get("id"))?
                                .as_str()?
                                .to_owned();
                            Some(GrokChoice {
                                name: effort
                                    .get("label")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or(&id)
                                    .to_owned(),
                                description: effort
                                    .get("description")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToOwned::to_owned),
                                id,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(GrokModel {
                name: model
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                description: model
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                default_effort: model
                    .pointer("/_meta/reasoningEffort")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                id,
                efforts,
            })
        })
        .collect::<Vec<_>>();
    let current_effort = models
        .iter()
        .find(|model| model.id == current_model_id)
        .and_then(|model| model.default_effort.clone());
    (!models.is_empty()).then_some(GrokModelState {
        current_model_id,
        current_effort,
        models,
    })
}

/// Present Grok's catalogue as the standard selectors consumed by Hel.
pub fn config_options(state: &GrokModelState) -> Vec<SessionConfigOption> {
    let choice = |choice: &GrokChoice| {
        let mut option = SessionConfigSelectOption::new(
            SessionConfigValueId::new(choice.id.clone()),
            choice.name.clone(),
        );
        option.description = choice.description.clone();
        option
    };
    let mut options = vec![{
        let models = state
            .models
            .iter()
            .map(|model| {
                choice(&GrokChoice {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    description: model.description.clone(),
                })
            })
            .collect::<Vec<_>>();
        let mut option = SessionConfigOption::new(
            SessionConfigId::new("model"),
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(state.current_model_id.clone()),
                SessionConfigSelectOptions::Ungrouped(models),
            )),
        );
        option.category = Some(SessionConfigOptionCategory::Model);
        option
    }];
    let efforts = state
        .current_model()
        .map(|model| model.efforts.as_slice())
        .unwrap_or_default();
    if let Some(current) = state.current_effort.clone()
        && !efforts.is_empty()
    {
        let mut option = SessionConfigOption::new(
            SessionConfigId::new("effort"),
            "Reasoning effort",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(current),
                SessionConfigSelectOptions::Ungrouped(
                    efforts.iter().map(choice).collect::<Vec<_>>(),
                ),
            )),
        );
        option.category = Some(SessionConfigOptionCategory::ThoughtLevel);
        options.push(option);
    }
    options
}

pub fn merge_config_options(options: &mut Vec<SessionConfigOption>, state: &GrokModelState) {
    for synthesized in config_options(state) {
        if let Some(existing) = options
            .iter_mut()
            .find(|option| option.id == synthesized.id)
        {
            *existing = synthesized;
        } else {
            options.push(synthesized);
        }
    }
}

pub fn handles_config_key(key: &str) -> bool {
    matches!(key, "model" | "effort")
}

pub fn set_model_request(
    session_id: &SessionId,
    state: &GrokModelState,
    key: &str,
    value: &str,
) -> Result<(serde_json::Value, GrokModelState)> {
    let mut updated = state.clone();
    let model_id = match key {
        "model" => {
            ensure!(
                state.models.iter().any(|model| model.id == value),
                "{value:?} is not an available model value"
            );
            updated.current_model_id = value.to_owned();
            updated.current_effort = state
                .models
                .iter()
                .find(|model| model.id == value)
                .and_then(|model| model.default_effort.clone());
            value.to_owned()
        }
        "effort" => {
            ensure!(
                state
                    .current_model()
                    .is_some_and(|model| model.efforts.iter().any(|effort| effort.id == value)),
                "{value:?} is not an available effort value"
            );
            updated.current_effort = Some(value.to_owned());
            state.current_model_id.clone()
        }
        _ => bail!("Grok Build has no {key} selector"),
    };
    let mut params = serde_json::Map::new();
    params.insert("sessionId".into(), session_id.to_string().into());
    params.insert("modelId".into(), model_id.into());
    if key == "effort" {
        params.insert(
            "_meta".into(),
            serde_json::json!({ "reasoningEffort": value }),
        );
    }
    Ok((serde_json::Value::Object(params), updated))
}

pub async fn apply_model_change(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    state: &mut GrokModelState,
    key: &str,
    value: &str,
) -> Result<()> {
    let (params, updated) = set_model_request(session_id, state, key, value)?;
    connection
        .send_request(
            agent_client_protocol::UntypedMessage::new(SET_MODEL_METHOD, params)
                .context("build Grok Build set-model request")?,
        )
        .block_task()
        .await
        .with_context(|| format!("set session {key} to {value}"))?;
    *state = updated;
    Ok(())
}

pub fn response_was_lost(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<agent_client_protocol::Error>()
            .is_some_and(|error| {
                error.code == agent_client_protocol::ErrorCode::InternalError
                    && error
                        .message
                        .starts_with("response to `session/set_model` never received:")
            })
    })
}

pub fn permits_unadvertised_plan_mode(harness: HarnessKind, mode_id: &str) -> bool {
    harness == HarnessKind::Grok && matches!(mode_id, "plan" | "default")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::acp::{find_session_config_option, select_contains};
    use crate::elicitation::ElicitationValue;

    fn model_meta() -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "modelState".into(),
            serde_json::json!({
                "currentModelId": "grok-4.6",
                "availableModels": [
                    {
                        "modelId": "grok-4.6",
                        "name": "Grok 4.6",
                        "_meta": {
                            "reasoningEffort": "high",
                            "reasoningEfforts": [
                                {"value": "xhigh", "label": "Extra High"},
                                {"value": "high", "label": "High"},
                                {"value": "medium", "label": "Medium"},
                                {"value": "low", "label": "Low"}
                            ]
                        }
                    },
                    {
                        "modelId": "grok-4.5",
                        "name": "Grok 4.5",
                        "_meta": {
                            "reasoningEffort": "high",
                            "reasoningEfforts": [
                                {"value": "high", "label": "High"},
                                {"value": "low", "label": "Low"}
                            ]
                        }
                    }
                ]
            }),
        );
        meta
    }

    fn select_values(option: &SessionConfigOption) -> Vec<String> {
        let SessionConfigKind::Select(select) = &option.kind else {
            panic!("expected a select option");
        };
        let SessionConfigSelectOptions::Ungrouped(options) = &select.options else {
            panic!("expected ungrouped options");
        };
        options
            .iter()
            .map(|option| option.value.to_string())
            .collect()
    }

    #[test]
    fn model_state_reads_each_models_default_effort() {
        let state = model_state(Some(&model_meta())).unwrap();

        assert_eq!(state.current_model_id, "grok-4.6");
        assert_eq!(state.current_effort.as_deref(), Some("high"));
        assert_eq!(state.models.len(), 2);
        assert_eq!(state.models[0].name, "Grok 4.6");
        assert_eq!(
            state.models[0]
                .efforts
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["xhigh", "high", "medium", "low"]
        );
        assert_eq!(state.models[1].default_effort.as_deref(), Some("high"));
        assert_eq!(model_state(None), None);
        assert_eq!(model_state(Some(&serde_json::Map::new())), None);
    }

    #[test]
    fn config_options_carry_the_current_model_and_its_effort_tiers() {
        let options = config_options(&model_state(Some(&model_meta())).unwrap());

        assert_eq!(select_values(&options[0]), ["grok-4.6", "grok-4.5"]);
        assert!(select_contains(&options[0].kind, "grok-4.5"));
        assert_eq!(
            select_values(&options[1]),
            ["xhigh", "high", "medium", "low"]
        );
        assert!(find_session_config_option(&options, "model").is_some());
        assert!(find_session_config_option(&options, "effort").is_some());
    }

    #[test]
    fn config_options_do_not_invent_an_unknown_effort() {
        let mut meta = model_meta();
        meta["modelState"]["availableModels"][0]["_meta"]
            .as_object_mut()
            .unwrap()
            .remove("reasoningEffort");

        let options = config_options(&model_state(Some(&meta)).unwrap());

        assert!(find_session_config_option(&options, "model").is_some());
        assert!(find_session_config_option(&options, "effort").is_none());
    }

    #[test]
    fn a_model_change_uses_the_new_models_advertised_default_effort() {
        let state = model_state(Some(&model_meta())).unwrap();
        let (params, updated) =
            set_model_request(&SessionId::from("s-1"), &state, "model", "grok-4.5").unwrap();

        assert_eq!(
            params,
            serde_json::json!({"sessionId": "s-1", "modelId": "grok-4.5"})
        );
        assert_eq!(updated.current_model_id, "grok-4.5");
        assert_eq!(updated.current_effort.as_deref(), Some("high"));
        assert_eq!(select_values(&config_options(&updated)[1]), ["high", "low"]);
    }

    #[test]
    fn an_effort_change_resends_the_current_model_with_effort_metadata() {
        let state = model_state(Some(&model_meta())).unwrap();
        let (params, updated) =
            set_model_request(&SessionId::from("s-1"), &state, "effort", "low").unwrap();

        assert_eq!(
            params,
            serde_json::json!({
                "sessionId": "s-1",
                "modelId": "grok-4.6",
                "_meta": {"reasoningEffort": "low"}
            })
        );
        assert_eq!(updated.current_effort.as_deref(), Some("low"));
    }

    #[test]
    fn invalid_model_changes_are_rejected_before_sending() {
        let state = model_state(Some(&model_meta())).unwrap();
        let session_id = SessionId::from("s-1");

        assert!(set_model_request(&session_id, &state, "model", "missing").is_err());
        assert!(set_model_request(&session_id, &state, "effort", "missing").is_err());
        assert!(set_model_request(&session_id, &state, "verbosity", "high").is_err());
    }

    #[test]
    fn real_config_options_survive_synthesized_model_refreshes() {
        let state = model_state(Some(&model_meta())).unwrap();
        let mut options = vec![SessionConfigOption::new(
            SessionConfigId::new("verbosity"),
            "Verbosity",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new("normal"),
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    "normal", "Normal",
                )]),
            )),
        )];

        merge_config_options(&mut options, &state);

        assert!(find_session_config_option(&options, "verbosity").is_some());
        assert!(find_session_config_option(&options, "model").is_some());
        assert!(find_session_config_option(&options, "effort").is_some());
    }

    #[test]
    fn extension_gates_include_the_harness_and_exact_values() {
        assert!(handles_exit_plan_mode(
            HarnessKind::Grok,
            "_x.ai/exit_plan_mode"
        ));
        assert!(!handles_exit_plan_mode(
            HarnessKind::Claude,
            "x.ai/exit_plan_mode"
        ));
        assert!(permits_unadvertised_plan_mode(HarnessKind::Grok, "plan"));
        assert!(!permits_unadvertised_plan_mode(HarnessKind::Claude, "plan"));
        assert!(!permits_unadvertised_plan_mode(HarnessKind::Grok, "agent"));
        assert_eq!(plan_review_id(7), "plan-review-grok-7");
        assert!(is_plan_review_id("plan-review-grok-7"));
        assert!(!is_plan_review_id("plan-review-codex-7"));
    }

    #[test]
    fn revise_without_feedback_omits_the_feedback_member() {
        let mut content = BTreeMap::new();
        content.insert("action".into(), ElicitationValue::String("revise".into()));

        assert_eq!(
            plan_response(ElicitationResponse::Accept { content }),
            serde_json::json!({"outcome": "cancelled"})
        );
    }

    #[test]
    fn response_loss_is_distinct_from_an_agents_explicit_rejection() {
        let lost = anyhow::Error::new(agent_client_protocol::Error::new(
            -32603,
            "response to `session/set_model` never received: channel closed",
        ));
        let rejected = anyhow::Error::new(agent_client_protocol::Error::invalid_params());

        assert!(response_was_lost(&lost));
        assert!(!response_was_lost(&rejected));
    }
}
