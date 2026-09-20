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
pub enum WaitingOn {
    User,
    BackgroundWork,
    StillWorking,
    Finished,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnVerdict {
    pub waiting_on: WaitingOn,
    pub confidence: f32,
    pub asked_question: f32,
}

impl TurnVerdict {
    /// Parse the HTTP response shape documented by TypeSafe, including Noul's object wrapper.
    pub fn parse(response: &Value) -> Result<Self> {
        let answers = response.get("answers").context("missing verdict answers")?;
        let choice = &answers["waiting_on"];
        let waiting_on = match choice["choice"]
            .as_str()
            .context("missing waiting_on choice")?
        {
            "user" => WaitingOn::User,
            "background_work" => WaitingOn::BackgroundWork,
            "still_working" => WaitingOn::StillWorking,
            "finished" => WaitingOn::Finished,
            _ => WaitingOn::Unclear,
        };
        Ok(Self {
            waiting_on,
            confidence: probability(&choice["confidence"])?,
            asked_question: probability(&answers["asked_question"]["noul"])?,
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
    if !verdict.confidence.is_finite() || !(ACT_CONFIDENCE..=1.0).contains(&verdict.confidence) {
        return Decision::KeepCurrent;
    }
    match (phase, verdict.waiting_on) {
        (TurnPhase::Running, WaitingOn::User) => Decision::AwaitingInput,
        (TurnPhase::Replied, WaitingOn::BackgroundWork) => Decision::ExpectContinuation,
        (TurnPhase::Replied, WaitingOn::Finished | WaitingOn::User) => Decision::InferIdle,
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
    fn decisions_require_the_right_phase_choice_and_confidence() {
        for phase in [TurnPhase::Running, TurnPhase::Replied] {
            for waiting_on in [
                WaitingOn::User,
                WaitingOn::BackgroundWork,
                WaitingOn::StillWorking,
                WaitingOn::Finished,
                WaitingOn::Unclear,
            ] {
                for confidence in [0.0, 0.849, 0.85, 0.99, 1.0, 1.01, f32::NAN] {
                    let verdict = TurnVerdict {
                        waiting_on,
                        confidence,
                        asked_question: 0.0,
                    };
                    let expected = match (phase, waiting_on) {
                        (TurnPhase::Running, WaitingOn::User)
                            if (ACT_CONFIDENCE..=1.0).contains(&confidence) =>
                        {
                            Decision::AwaitingInput
                        }
                        (TurnPhase::Replied, WaitingOn::BackgroundWork)
                            if (ACT_CONFIDENCE..=1.0).contains(&confidence) =>
                        {
                            Decision::ExpectContinuation
                        }
                        (TurnPhase::Replied, WaitingOn::Finished | WaitingOn::User)
                            if (ACT_CONFIDENCE..=1.0).contains(&confidence) =>
                        {
                            Decision::InferIdle
                        }
                        _ => Decision::KeepCurrent,
                    };
                    assert_eq!(decide(phase, &verdict), expected);
                }
            }
        }
    }

    #[test]
    fn documented_response_parses_and_invalid_probabilities_fail_closed() {
        let mut response = json!({"answers":{"waiting_on":{"type":"choice","choice":"user","confidence":0.95,"probabilities":{"user":0.99}},"asked_question":{"type":"noul","noul":0.97}}});
        let verdict = TurnVerdict::parse(&response).unwrap();
        assert_eq!(verdict.waiting_on, WaitingOn::User);
        assert_eq!(verdict.asked_question, 0.97);
        response["answers"]["waiting_on"]["choice"] = json!("future_choice");
        assert_eq!(
            TurnVerdict::parse(&response).unwrap().waiting_on,
            WaitingOn::Unclear
        );
        response["answers"]["waiting_on"]["confidence"] = json!(1.01);
        assert!(TurnVerdict::parse(&response).is_err());
        response["answers"]["waiting_on"]["confidence"] = json!(0.95);
        response["answers"]["asked_question"]["noul"] = json!(-0.1);
        assert!(TurnVerdict::parse(&response).is_err());
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
