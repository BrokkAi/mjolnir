//! Choices a draft offers only once they are discovered: the models and
//! efforts its profiles advertise, from the daemon's capability catalog.
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
use serde_json::Value;

/// Set on a task's discovered choices while its selected model's efforts
/// are still being discovered.
pub const EFFORTS_PENDING: &str = "efforts_pending";

/// Fill the sub-agent model and effort choices of `draft` from
/// `capabilities`. A task whose profiles are not all discovered yet is left
/// unset, which the page reports as still discovering. Returns what
/// discovery could not reach; a draft that is not a configuration yet is
/// left as it is.
pub fn fill_subagent_options(
    draft: &mut Value,
    capabilities: &ProfileCapabilitiesSnapshot,
) -> Result<Option<String>, serde_json::Error> {
    let config = super::draft::config_from_draft(draft.clone())?;
    let mut unreachable = None;
    for task in ["straightforward", "subtle", "oracle"] {
        let model = draft["subagents"][task]["model"].as_str();
        let mut options = capabilities.options(&config, None);
        let mut efforts_pending = false;
        if let Some(options) = &mut options {
            match capabilities.options(&config, model) {
                Some(efforts) => {
                    options.efforts = efforts.efforts;
                    options.unavailable = efforts.unavailable;
                }
                // The selected model's efforts are still being discovered.
                None => efforts_pending = true,
            }
        }
        draft["_subagent_options"][task] = match options {
            Some(options) => {
                if !options.unavailable.is_empty() {
                    unreachable = Some(format!("Discovery: {}", options.unavailable.join("; ")));
                }
                let mut options = serde_json::to_value(options).expect("options serialize");
                if efforts_pending
                    || options["unavailable"].as_array().is_some_and(|reasons| {
                        reasons.iter().any(|reason| {
                            reason
                                .as_str()
                                .is_some_and(|reason| reason.contains("pending"))
                        })
                    })
                {
                    options[EFFORTS_PENDING] = Value::Bool(true);
                }
                options
            }
            None => Value::Null,
        };
    }
    Ok(unreachable)
}

/// Fill reviewer choices from the same profile catalog as subagent choices.
pub fn fill_reviewer_options(
    draft: &mut Value,
    capabilities: &ProfileCapabilitiesSnapshot,
) -> Result<(), serde_json::Error> {
    let config = super::draft::config_from_draft(draft.clone())?;
    let Some(reviewers) = draft["review"]["reviewers"].as_array() else {
        return Ok(());
    };
    let models = capabilities.options(&config, None);
    let mut options = serde_json::Map::new();
    for (index, reviewer) in reviewers.iter().enumerate() {
        let model = reviewer["model"].as_str().filter(|model| !model.is_empty());
        let mut current = models.clone();
        if let Some(current) = &mut current {
            match capabilities.options(&config, model) {
                Some(efforts) => {
                    current.efforts = efforts.efforts;
                    current.unavailable = efforts.unavailable;
                }
                None => {
                    let mut value = serde_json::to_value(current).expect("options serialize");
                    value[EFFORTS_PENDING] = Value::Bool(true);
                    options.insert(index.to_string(), value);
                    continue;
                }
            }
        }
        let mut value = serde_json::to_value(current).expect("options serialize");
        if value["unavailable"].as_array().is_some_and(|reasons| {
            reasons.iter().any(|reason| {
                reason
                    .as_str()
                    .is_some_and(|reason| reason.contains("pending"))
            })
        }) {
            value[EFFORTS_PENDING] = Value::Bool(true);
        }
        options.insert(index.to_string(), value);
    }
    draft["_reviewer_options"] = Value::Object(options);
    Ok(())
}
