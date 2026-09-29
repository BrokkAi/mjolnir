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
pub const SERVER_RETRY_CONFIDENCE: f32 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Replied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ToolEvidence {
    pub title: String,
    pub running_s: u64,
}

/// One background command the agent left running, as the quiet judgment sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct BackgroundEvidence {
    pub id: String,
    /// The command line, cut to [`TOOL_TITLE_BYTES`].
    pub command: String,
    pub started_s_ago: u64,
}

/// A tool call the agent made after its last text, by name and outcome only.
/// A `handback` or `spawn` here says what the reply's silence does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ToolOutcome {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TurnEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<crate::assessment::ContextHistory>,
    /// Tool calls made after the last assistant text of the turn, oldest
    /// first, at most [`IN_FLIGHT_TOOLS`]. Empty on older workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub final_tool_calls: Vec<ToolOutcome>,
    /// The background commands behind `background_commands`, sent so Jev can
    /// judge whether anyone still depends on them. Empty on older workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<BackgroundEvidence>,
    pub harness: HarnessKind,
    pub phase: TurnPhase,
    pub silent_for_s: u64,
    pub tools_in_flight: Vec<ToolEvidence>,
    pub transcript_summary: String,
    pub background_commands: usize,
    pub queued_commands: usize,
    pub user_prompt_tail: String,
    pub assistant_text_tail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CompletionEvidence {
    pub stop_reason: String,
    pub diagnostic: Option<crate::diagnostic::TurnDiagnostic>,
}

impl CompletionEvidence {
    pub fn bounded(
        stop_reason: &str,
        diagnostic: Option<&crate::diagnostic::TurnDiagnostic>,
    ) -> Self {
        let mut stop_reason = stop_reason.to_owned();
        stop_reason.truncate(stop_reason.floor_char_boundary(128));
        let diagnostic = diagnostic.cloned().map(|mut d| {
            if d.message.len() > 4096 {
                d.message = "[Provider diagnostic omitted: exceeds 4096 bytes]".into();
            }
            if d.code.as_ref().is_some_and(|v| v.len() > 128) {
                d.code = None;
            }
            if d.reset_at.as_ref().is_some_and(|v| v.len() > 256) {
                d.reset_at = None;
            }
            d
        });
        Self {
            stop_reason,
            diagnostic,
        }
    }
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
    pub assessment: Option<crate::assessment::Verdict>,
    pub work_state: WorkState,
    pub work_state_confidence: f32,
    pub needs_user_input: f32,
    pub retryable_server_error: Option<f32>,
}

impl TurnVerdict {
    /// Parse the HTTP response shape documented by TypeSafe, including Noul's object wrapper.
    pub fn parse(response: &Value) -> Result<Self> {
        if response["answers"].get("failure").is_some() {
            use crate::assessment::{Failure, Input, Work};
            let v = crate::assessment::Verdict::parse(response)?;
            return Ok(Self {
                assessment: Some(v),
                work_state: match v.work.choice {
                    Work::Finished => WorkState::Finished,
                    Work::Waiting => WorkState::BackgroundWork,
                    Work::AuthorizedUnfinished => WorkState::StillWorking,
                    Work::Unclear => WorkState::Unclear,
                },
                work_state_confidence: v.work.confidence,
                needs_user_input: match v.input.choice {
                    Input::Required => v.input.confidence,
                    Input::None | Input::RedundantRequest => 1.0 - v.input.confidence,
                    Input::Unclear => 0.5,
                },
                retryable_server_error: Some(if v.failure.choice == Failure::TransientProvider {
                    v.failure.confidence
                } else {
                    0.0
                }),
            });
        }
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
            assessment: None,
            work_state,
            work_state_confidence: probability(&choice["confidence"])?,
            needs_user_input: probability(&answers["needs_user_input"]["noul"])?,
            retryable_server_error: match answers.get("retryable_server_error") {
                Some(answer) => {
                    ensure!(answer["type"] == "noul", "invalid retry answer type");
                    Some(probability(&answer["noul"])?)
                }
                None => None,
            },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AwaitingInput,
    ExpectContinuation,
    InferIdle,
    KeepCurrent,
}

/// A worker-owned completion decision for one physical prompt boundary.
/// KeepCurrent preserves the harness outcome; other decisions are Jev verdicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TurnCompletion {
    pub command_id: String,
    /// Transcript frontier the completion decision covers, including autonomous work.
    pub completed_ordinal: u64,
    pub decision: Decision,
}

pub fn decide(phase: TurnPhase, verdict: &TurnVerdict) -> Decision {
    if !(0.0..=1.0).contains(&verdict.needs_user_input) {
        return Decision::KeepCurrent;
    }
    if verdict.needs_user_input >= ACT_CONFIDENCE {
        return Decision::AwaitingInput;
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
                            assessment: None,
                            work_state,
                            work_state_confidence,
                            needs_user_input,
                            retryable_server_error: None,
                        };
                        let expected = if !(0.0..=1.0).contains(&needs_user_input) {
                            Decision::KeepCurrent
                        } else if needs_user_input >= 0.85 {
                            Decision::AwaitingInput
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
