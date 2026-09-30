use super::*;

pub fn viewer_config_options(
    config_options: &[agent_client_protocol::schema::v1::SessionConfigOption],
    facts: &mj_core::acp::AcpSessionFacts,
) -> Vec<ViewerConfigOption> {
    // Keep canonical model/effort aliases, then publish the other selectors
    // by their advertised ids. API validation must see the full catalogue.
    let aliases = ["model", "effort"];
    let aliased_ids: Vec<_> = aliases
        .iter()
        .filter_map(|key| mj_core::acp::find_session_config_option(config_options, key))
        .map(|option| option.id.to_string())
        .collect();
    let keys: Vec<_> = aliases
        .into_iter()
        .map(str::to_owned)
        .chain(
            config_options
                .iter()
                .map(|option| option.id.to_string())
                .filter(|id| !aliased_ids.contains(id)),
        )
        .collect();
    keys.into_iter()
        .filter_map(|key| {
            let choices = mj_core::acp::session_config_choices(config_options, &key);
            if choices.is_empty() {
                return None;
            }
            Some(ViewerConfigOption {
                key: key.to_owned(),
                label: key.to_owned(),
                current: match key.as_str() {
                    "model" => facts.current_model(),
                    "effort" => facts.current_effort(),
                    _ => None,
                }
                .map(str::to_owned)
                .or_else(|| mj_core::acp::surface::config_current_value(config_options, &key)),
                choices: choices
                    .into_iter()
                    .map(|choice| ViewerConfigChoice {
                        value: choice.value,
                        name: choice.name,
                        description: choice.description,
                    })
                    .collect(),
            })
        })
        .collect()
}

pub fn session_config_view(
    harness: mj_core::config::HarnessKind,
    state: &mj_core::relay::RelayOperationalState,
) -> Vec<ViewerConfigOption> {
    // Selectors are ACP categories; the config map supplies legacy current values.
    let facts = mj_core::acp::AcpSessionFacts::from_operational(
        harness,
        &state.config,
        &state.config_options,
        state.modes.as_ref(),
    );
    viewer_config_options(&state.config_options, &facts)
}
