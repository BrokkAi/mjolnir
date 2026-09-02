//! Normalized session controls exposed by an ACP agent.
//!
//! ACP agents can advertise the same user-facing control through configuration
//! options, legacy session modes, or provider extensions.  This module is the
//! single place where Hel turns those protocol surfaces into chat semantics.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    AvailableCommand, SessionConfigKind, SessionConfigOption, SessionModeState,
};
use serde_json::Value;

use crate::hel_config::HarnessKind;

use super::{dialect::grok, find_session_config_option, select_contains};

const FAST_MODE_CONFIG_ID: &str = "fast-mode";
const FAST_MODE_ON: &str = "on";
const FAST_MODE_OFF: &str = "off";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanControl {
    SetConfig { key: String, value: String },
    SetSessionMode { mode_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanControlError {
    DeepseekUnsupported,
    CodexIncompatible,
    GrokIncompatible,
    Incompatible,
}

/// ACP controls and commands available to one live session.
#[derive(Debug, Clone, Default)]
pub struct AcpSessionSurface {
    harness_kind: Option<HarnessKind>,
    config_options: Vec<SessionConfigOption>,
    session_modes: Option<SessionModeState>,
    current_mode: Option<String>,
    agent_commands: Vec<AvailableCommand>,
    current_model: Option<String>,
    current_effort: Option<String>,
    plan_mode_change_pending: bool,
}

impl AcpSessionSurface {
    pub fn from_configuration(configuration: &BTreeMap<String, Value>) -> Self {
        Self {
            current_mode: configuration
                .get("collaboration_mode")
                .or_else(|| configuration.get("mode"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            current_model: configuration
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned),
            current_effort: configuration
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Self::default()
        }
    }

    pub fn set_harness_kind(&mut self, harness_kind: HarnessKind) {
        self.harness_kind = Some(harness_kind);
        self.sync_plan_mode();
    }

    pub fn set_config_options(&mut self, options: &[SessionConfigOption]) {
        self.config_options = options.to_vec();
        self.current_model = config_current_value(options, "model");
        self.current_effort = config_current_value(options, "effort");
        self.sync_plan_mode();
    }

    pub fn set_session_modes(&mut self, modes: Option<SessionModeState>) {
        self.session_modes = modes;
        self.sync_plan_mode();
    }

    pub fn apply_current_mode_update(&mut self, mode: String) {
        if self.mode_config_key() == "collaboration_mode" {
            return;
        }
        self.current_mode = Some(mode.clone());
        if let Some(modes) = self.session_modes.as_mut() {
            modes.current_mode_id = mode.into();
        }
    }

    pub fn apply_projected_configuration(&mut self, configuration: &BTreeMap<String, Value>) {
        if let Some(mode) = configuration
            .get(self.mode_config_key())
            .and_then(Value::as_str)
        {
            self.current_mode = Some(mode.to_owned());
        }
        self.current_model = configuration
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| config_current_value(&self.config_options, "model"));
        self.current_effort = configuration
            .get("effort")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| config_current_value(&self.config_options, "effort"));
    }

    pub fn set_agent_commands(&mut self, commands: Vec<AvailableCommand>) {
        self.agent_commands = commands;
    }

    pub fn agent_commands(&self) -> &[AvailableCommand] {
        &self.agent_commands
    }

    pub fn advertises_command(&self, name: &str) -> bool {
        self.agent_commands
            .iter()
            .any(|command| command.name == name)
    }

    pub fn current_model(&self) -> Option<&str> {
        self.current_model.as_deref()
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.current_effort.as_deref()
    }

    pub fn supports_fast_mode(&self) -> bool {
        self.config_options.iter().any(|option| {
            option.id.to_string() == FAST_MODE_CONFIG_ID
                && select_contains(&option.kind, FAST_MODE_ON)
                && select_contains(&option.kind, FAST_MODE_OFF)
        })
    }

    pub fn fast_mode_active(&self) -> bool {
        self.supports_fast_mode()
            && config_current_value(&self.config_options, FAST_MODE_CONFIG_ID).as_deref()
                == Some(FAST_MODE_ON)
    }

    pub fn current_mode(&self) -> Option<&str> {
        self.current_mode.as_deref()
    }

    pub fn begin_plan_mode_change(&mut self, active: bool) {
        self.plan_mode_change_pending = true;
        self.current_mode = Some(if active { "plan" } else { "default" }.into());
    }

    pub fn finish_plan_mode_change(&mut self, active: bool) {
        self.plan_mode_change_pending = false;
        self.current_mode = Some(if active { "plan" } else { "default" }.into());
    }

    pub fn supports_plan_mode(&self) -> bool {
        self.plan_control(true).is_ok()
    }

    pub fn plan_mode_active(&self) -> bool {
        self.supports_plan_mode() && self.current_mode() == Some("plan")
    }

    pub fn plan_control(&self, active: bool) -> Result<PlanControl, PlanControlError> {
        let value = if active { "plan" } else { "default" };
        match self.harness_kind {
            Some(HarnessKind::Deepseek) => Err(PlanControlError::DeepseekUnsupported),
            Some(HarnessKind::Codex) => self
                .exact_config_has_plan_pair("collaboration_mode")
                .then(|| PlanControl::SetConfig {
                    key: "collaboration_mode".into(),
                    value: value.into(),
                })
                .ok_or(PlanControlError::CodexIncompatible),
            Some(HarnessKind::Claude | HarnessKind::Kimi) => {
                if self.exact_config_has_plan_pair("mode") {
                    Ok(PlanControl::SetConfig {
                        key: "mode".into(),
                        value: value.into(),
                    })
                } else if self.advertised_plan_modes() {
                    Ok(PlanControl::SetSessionMode {
                        mode_id: value.into(),
                    })
                } else {
                    Err(PlanControlError::Incompatible)
                }
            }
            Some(harness @ HarnessKind::Grok)
                if grok::permits_unadvertised_plan_mode(harness, value) =>
            {
                Ok(PlanControl::SetSessionMode {
                    mode_id: value.into(),
                })
            }
            Some(HarnessKind::Grok) => Err(PlanControlError::GrokIncompatible),
            None => {
                if self.config_has_plan_pair("mode") {
                    Ok(PlanControl::SetConfig {
                        key: "mode".into(),
                        value: value.into(),
                    })
                } else if self.advertised_plan_modes() {
                    Ok(PlanControl::SetSessionMode {
                        mode_id: value.into(),
                    })
                } else {
                    Err(PlanControlError::Incompatible)
                }
            }
        }
    }

    fn sync_plan_mode(&mut self) {
        if self.plan_mode_change_pending {
            return;
        }
        if let Some(value) = config_current_value(&self.config_options, self.mode_config_key()) {
            self.current_mode = Some(value);
        } else if self.harness_kind != Some(HarnessKind::Codex) {
            self.current_mode = self
                .session_modes
                .as_ref()
                .map(|modes| modes.current_mode_id.to_string());
        }
    }

    fn mode_config_key(&self) -> &'static str {
        match self.harness_kind {
            Some(HarnessKind::Codex) => "collaboration_mode",
            _ => "mode",
        }
    }

    fn advertised_plan_modes(&self) -> bool {
        self.session_modes.as_ref().is_some_and(|modes| {
            ["plan", "default"].into_iter().all(|desired| {
                modes
                    .available_modes
                    .iter()
                    .any(|mode| mode.id.to_string() == desired)
            })
        })
    }

    fn config_has_plan_pair(&self, key: &str) -> bool {
        find_session_config_option(&self.config_options, key).is_some_and(|option| {
            select_contains(&option.kind, "plan") && select_contains(&option.kind, "default")
        })
    }

    fn exact_config_has_plan_pair(&self, key: &str) -> bool {
        self.config_options.iter().any(|option| {
            option.id.to_string() == key
                && select_contains(&option.kind, "plan")
                && select_contains(&option.kind, "default")
        })
    }
}

pub(crate) fn config_current_value(options: &[SessionConfigOption], key: &str) -> Option<String> {
    let option = find_session_config_option(options, key)?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.to_string())
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        SessionConfigId, SessionConfigSelect, SessionConfigSelectOption,
        SessionConfigSelectOptions, SessionConfigValueId,
    };

    use super::*;

    fn mode_option(key: &str, current: &str) -> SessionConfigOption {
        SessionConfigOption::new(
            SessionConfigId::new(key),
            "Mode",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(current),
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ]),
            )),
        )
    }

    fn fast_mode_option(current: &str) -> SessionConfigOption {
        SessionConfigOption::new(
            SessionConfigId::new(FAST_MODE_CONFIG_ID),
            "Fast mode",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(current),
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new(FAST_MODE_OFF, "Off"),
                    SessionConfigSelectOption::new(FAST_MODE_ON, "On"),
                ]),
            )),
        )
    }

    #[test]
    fn config_churn_does_not_revert_an_in_flight_plan_change() {
        let mut surface = AcpSessionSurface::default();
        surface.set_harness_kind(HarnessKind::Claude);
        surface.set_config_options(&[mode_option("mode", "default")]);
        surface.begin_plan_mode_change(true);

        let mut churned = mode_option("mode", "default");
        churned.description = Some("metadata changed".into());
        surface.set_config_options(&[churned]);

        assert_eq!(surface.current_mode(), Some("plan"));
        surface.finish_plan_mode_change(true);
        assert_eq!(surface.current_mode(), Some("plan"));
    }

    #[test]
    fn codex_uses_one_mode_key_for_snapshots_options_and_projection() {
        let mut surface = AcpSessionSurface::from_configuration(&BTreeMap::from([
            ("mode".into(), Value::String("default".into())),
            ("collaboration_mode".into(), Value::String("plan".into())),
        ]));
        surface.set_harness_kind(HarnessKind::Codex);
        assert_eq!(surface.current_mode(), Some("plan"));

        surface.set_config_options(&[mode_option("collaboration_mode", "default")]);
        assert_eq!(surface.current_mode(), Some("default"));
        surface.apply_projected_configuration(&BTreeMap::from([(
            "collaboration_mode".into(),
            Value::String("plan".into()),
        )]));
        assert_eq!(surface.current_mode(), Some("plan"));
    }

    #[test]
    fn fast_mode_requires_the_codex_selector_and_tracks_its_current_value() {
        let mut surface = AcpSessionSurface::default();
        assert!(!surface.supports_fast_mode());
        assert!(!surface.fast_mode_active());

        surface.set_config_options(&[fast_mode_option(FAST_MODE_OFF)]);
        assert!(surface.supports_fast_mode());
        assert!(!surface.fast_mode_active());

        surface.set_config_options(&[fast_mode_option(FAST_MODE_ON)]);
        assert!(surface.supports_fast_mode());
        assert!(surface.fast_mode_active());

        surface.set_config_options(&[]);
        assert!(!surface.supports_fast_mode());
        assert!(!surface.fast_mode_active());
    }
}
