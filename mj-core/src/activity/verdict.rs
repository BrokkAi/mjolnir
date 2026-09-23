//! Bounded turn evidence and conservative decisions for the optional Jev classifier.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::config::HarnessKind;

pub const USER_PROMPT_BYTES: usize = 1024;
pub const ASSISTANT_TEXT_BYTES: usize = 2048;
pub const TOOL_TITLE_BYTES: usize = 128;
pub const IN_FLIGHT_TOOLS: usize = 16;
/// Jev recommends high confidence for automation; weaker answers preserve current behavior.
pub const ACT_CONFIDENCE: f32 = 0.85;
pub const NO_INPUT_CONFIDENCE: f32 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Replied,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolEvidence {
    pub title: String,
    pub running_s: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnEvidence {
    pub harness: HarnessKind,
    pub phase: TurnPhase,
    pub silent_for_s: u64,
    pub tools_in_flight: Vec<ToolEvidence>,
    pub transcript_summary: String,
    pub background_commands: usize,
    pub queued_commands: usize,
    pub user_prompt_tail: String,
    pub assistant_text_tail: String,
}

pub fn questions() -> Value {
    serde_json::from_str(include_str!("verdict_questions.json"))
        .expect("bundled turn verdict questions are valid JSON")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    BackgroundWork,
    StillWorking,
    Finished,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnVerdict {
    pub work_state: WorkState,
    pub work_state_confidence: f32,
    pub needs_user_input: f32,
}

impl TurnVerdict {
    /// Parse the HTTP response shape documented by TypeSafe, including Noul's object wrapper.
    pub fn parse(response: &Value) -> Result<Self> {
        let answers = response.get("answers").context("missing verdict answers")?;
        let choice = &answers["work_state"];
        ensure!(
            choice["type"] == "choice" && answers["needs_user_input"]["type"] == "noul",
            "invalid verdict answer types"
        );
        let choice_text = choice["choice"]
            .as_str()
            .context("missing work_state choice")?;
        ensure!(choice_text.len() <= 64, "invalid work_state choice");
        let work_state = match choice_text {
            "background_work" => WorkState::BackgroundWork,
            "still_working" => WorkState::StillWorking,
            "finished" => WorkState::Finished,
            _ => WorkState::Unclear,
        };
        Ok(Self {
            work_state,
            work_state_confidence: probability(&choice["confidence"])?,
            needs_user_input: probability(&answers["needs_user_input"]["noul"])?,
        })
    }
}

fn probability(value: &Value) -> Result<f32> {
    let number = value.as_f64().context("missing verdict probability")?;
    ensure!(
        number.is_finite() && (0.0..=1.0).contains(&number),
        "invalid verdict probability"
    );
    Ok(number as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AwaitingInput,
    ExpectContinuation,
    InferIdle,
    KeepCurrent,
}

pub fn decide(phase: TurnPhase, verdict: &TurnVerdict) -> Decision {
    if !(0.0..=1.0).contains(&verdict.needs_user_input) {
        return Decision::KeepCurrent;
    }
    if verdict.needs_user_input >= ACT_CONFIDENCE {
        return match phase {
            TurnPhase::Running => Decision::AwaitingInput,
            TurnPhase::Replied => Decision::InferIdle,
        };
    }
    if phase == TurnPhase::Running
        || verdict.needs_user_input > NO_INPUT_CONFIDENCE
        || !(ACT_CONFIDENCE..=1.0).contains(&verdict.work_state_confidence)
    {
        return Decision::KeepCurrent;
    }
    match verdict.work_state {
        WorkState::BackgroundWork => Decision::ExpectContinuation,
        WorkState::Finished => Decision::InferIdle,
        _ => Decision::KeepCurrent,
    }
}

/// Resolve once during startup, outside an event or render loop. Never log this value.
pub fn api_key() -> Option<String> {
    resolve_key(
        std::env::var("TYPESAFE_API_KEY").ok().as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

fn resolve_key(environment: Option<&str>, home: Option<&Path>) -> Option<String> {
    fn nonblank(value: &str) -> Option<String> {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }
    environment.and_then(nonblank).or_else(|| {
        let value =
            std::fs::read_to_string(home?.join(".secrets").join("typesafe_api_key")).ok()?;
        nonblank(&value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn independent_input_need_takes_precedence_over_background_work() {
        for phase in [TurnPhase::Running, TurnPhase::Replied] {
            for work_state in [
                WorkState::BackgroundWork,
                WorkState::StillWorking,
                WorkState::Finished,
                WorkState::Unclear,
            ] {
                for needs_user_input in [0.0, 0.15, 0.151, 0.849, 0.85, 1.0, -0.1, 1.1, f32::NAN] {
                    for work_state_confidence in [0.0, 0.849, 0.85, 1.0, 1.1, f32::NAN] {
                        let verdict = TurnVerdict {
                            work_state,
                            work_state_confidence,
                            needs_user_input,
                        };
                        let expected = if !(0.0..=1.0).contains(&needs_user_input) {
                            Decision::KeepCurrent
                        } else if needs_user_input >= 0.85 {
                            if phase == TurnPhase::Running {
                                Decision::AwaitingInput
                            } else {
                                Decision::InferIdle
                            }
                        } else if phase == TurnPhase::Replied
                            && needs_user_input <= 0.15
                            && (0.85..=1.0).contains(&work_state_confidence)
                        {
                            match work_state {
                                WorkState::BackgroundWork => Decision::ExpectContinuation,
                                WorkState::Finished => Decision::InferIdle,
                                _ => Decision::KeepCurrent,
                            }
                        } else {
                            Decision::KeepCurrent
                        };
                        assert_eq!(decide(phase, &verdict), expected, "{phase:?}: {verdict:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn documented_response_parses_and_malformed_responses_fail_closed() {
        let response = json!({"answers":{"work_state":{"type":"choice","choice":"background_work","confidence":0.95},"needs_user_input":{"type":"noul","noul":0.97}}});
        let verdict = TurnVerdict::parse(&response).unwrap();
        assert_eq!(verdict.work_state, WorkState::BackgroundWork);
        assert_eq!(verdict.needs_user_input, 0.97);
        assert_eq!(
            decide(TurnPhase::Running, &verdict),
            Decision::AwaitingInput
        );
        for value in [json!(-0.1), json!(1.01), json!(null), json!("0.95")] {
            for (field, score) in [("work_state", "confidence"), ("needs_user_input", "noul")] {
                let mut malformed = response.clone();
                malformed["answers"][field][score] = value.clone();
                assert!(TurnVerdict::parse(&malformed).is_err());
            }
        }
        assert!(TurnVerdict::parse(&json!({})).is_err());
    }

    #[test]
    fn key_resolution_prefers_nonblank_environment_and_trims_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".secrets")).unwrap();
        let file = home.path().join(".secrets/typesafe_api_key");
        std::fs::write(&file, " file-key\n").unwrap();
        assert_eq!(
            resolve_key(Some(" env-key \n"), Some(home.path())).as_deref(),
            Some("env-key")
        );
        assert_eq!(
            resolve_key(Some("  "), Some(home.path())).as_deref(),
            Some("file-key")
        );
        assert_eq!(
            resolve_key(None, Some(home.path())).as_deref(),
            Some("file-key")
        );
        std::fs::write(&file, " \n").unwrap();
        assert_eq!(resolve_key(None, Some(home.path())), None);
        assert_eq!(resolve_key(None, None), None);
    }
}
