//! The Codex model catalog and the rule that picks a Guardian reviewer model.
//!
//! Codex advertises the models a session may pick from a JSON file named by the
//! top-level `model_catalog_json` key in its `config.toml`. The file is an
//! object with a `models` array; each entry is an object whose `slug` is the
//! model name. Codex fetches this list from its own service only for ChatGPT
//! logins, so a profile that authenticates with an API key against another
//! provider would otherwise advertise OpenAI's built-in model names. Mjolnir
//! therefore fetches the provider's catalog and stages it as `models.json`.
//!
//! "Guardian" is Codex's review mode: before an action leaves the sandbox a
//! second model reviews it and returns an allow or deny outcome. Codex picks
//! that reviewer from the session model's catalog entry field
//! `auto_review_model_override`, falling back to reviewing with the session
//! model itself. Mjolnir stamps the override so a heavyweight session model is
//! not also its own reviewer.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// A Codex `models.json` document. Entries are kept as JSON objects so unknown
/// Codex fields pass through unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCatalog {
    pub models: Vec<Map<String, Value>>,
}

impl CodexCatalog {
    /// Every entry's `slug`, in catalog order.
    pub fn slugs(&self) -> Vec<String> {
        self.models
            .iter()
            .filter_map(|model| model.get("slug")?.as_str().map(str::to_owned))
            .collect()
    }

    /// Serialize back to the `{"models": [...]}` document Codex reads.
    pub fn to_json(&self) -> String {
        let models = Value::Array(self.models.iter().cloned().map(Value::Object).collect());
        let mut document = Map::new();
        document.insert("models".to_owned(), models);
        serde_json::to_string_pretty(&Value::Object(document))
            .expect("catalog objects are serializable")
    }
}

/// Parse a provider's `GET /models` response body, which uses the same shape as
/// Codex's `model_catalog_json` file.
pub fn parse(bytes: &[u8]) -> Result<CodexCatalog> {
    let value: Value = serde_json::from_slice(bytes).context("parse model catalog as JSON")?;
    let Some(models) = value.get("models").and_then(Value::as_array) else {
        bail!("model catalog has no `models` array");
    };
    let mut entries = Vec::with_capacity(models.len());
    for model in models {
        let Some(object) = model.as_object() else {
            bail!("model catalog entry is not a JSON object");
        };
        entries.push(object.clone());
    }
    if entries.is_empty() {
        bail!("model catalog lists no models");
    }
    Ok(CodexCatalog { models: entries })
}

/// The newest flash model among `slugs`, which Mjolnir uses as the Guardian
/// reviewer.
///
/// "Flash" is the vendor's name for its small, fast model; reviewing with it
/// keeps Guardian checks cheap. Versions compare as the dotted number sequence
/// that follows the first `-` in the slug, segment by segment and numerically,
/// so `glm-5.10-flash` is newer than `glm-5.3-flash`. Slugs with no flash marker
/// are ignored, and `None` means the catalog has no flash model and Codex should
/// keep its own fallback of reviewing with the session model.
pub fn guardian_review_model(slugs: impl IntoIterator<Item = String>) -> Option<String> {
    slugs
        .into_iter()
        .filter(|slug| slug.to_ascii_lowercase().contains("flash"))
        .max_by(|left, right| version_of(left).cmp(&version_of(right)))
}

/// The dotted number sequence in a slug, as numbers. `glm-5.3-flash` is
/// `[5, 3]`; a slug with no numbers is an empty vector, which sorts first.
fn version_of(slug: &str) -> Vec<u64> {
    slug.split(|character: char| !character.is_ascii_digit() && character != '.')
        .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|part| {
            part.split('.')
                .filter_map(|segment| segment.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Set `auto_review_model_override` on every catalog entry, so whichever model
/// the session runs on reviews with `reviewer`.
pub fn stamp_reviewer(catalog: &mut CodexCatalog, reviewer: &str) {
    for model in &mut catalog.models {
        model.insert(
            "auto_review_model_override".to_owned(),
            Value::from(reviewer),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"models":[
        {"slug":"glm-5.3","supported_reasoning_levels":["low","high","max"],"display_name":"GLM 5.3"},
        {"slug":"glm-5.3-flash","supported_reasoning_levels":["low","high","max"]},
        {"slug":"glm-5-turbo","some_future_field":{"nested":true}}
    ]}"#;

    #[test]
    fn parse_keeps_unknown_fields_and_reports_slugs() {
        let catalog = parse(BODY).expect("parse");
        assert_eq!(catalog.slugs(), ["glm-5.3", "glm-5.3-flash", "glm-5-turbo"]);
        assert_eq!(
            catalog.models[2]["some_future_field"]["nested"],
            Value::Bool(true)
        );
        assert_eq!(
            catalog.models[0]["display_name"],
            Value::from("GLM 5.3"),
            "unread fields survive a round trip"
        );
    }

    #[test]
    fn parse_rejects_a_body_without_models() {
        assert!(parse(br#"{"data":[]}"#).is_err());
        assert!(parse(br#"{"models":[]}"#).is_err());
    }

    #[test]
    fn guardian_reviewer_picks_the_newest_flash_model() {
        let picked = guardian_review_model(
            ["glm-5.3", "glm-5.2-flash", "glm-5.3-flash", "glm-5-turbo"].map(str::to_owned),
        );
        assert_eq!(picked.as_deref(), Some("glm-5.3-flash"));
    }

    #[test]
    fn guardian_reviewer_compares_versions_numerically_not_as_text() {
        let picked = guardian_review_model(["glm-5.3-flash", "glm-5.10-flash"].map(str::to_owned));
        assert_eq!(picked.as_deref(), Some("glm-5.10-flash"));
    }

    #[test]
    fn guardian_reviewer_is_absent_when_no_model_is_a_flash_model() {
        let picked = guardian_review_model(["glm-5.3", "glm-5-turbo"].map(str::to_owned));
        assert_eq!(picked, None);
    }

    #[test]
    fn stamping_the_reviewer_marks_every_entry_and_survives_serialization() {
        let mut catalog = parse(BODY).expect("parse");
        let reviewer = guardian_review_model(catalog.slugs()).expect("flash model");
        stamp_reviewer(&mut catalog, &reviewer);
        let restaged = parse(catalog.to_json().as_bytes()).expect("reparse");
        for model in &restaged.models {
            assert_eq!(model["auto_review_model_override"], Value::from(&*reviewer));
        }
        assert_eq!(restaged.models[2]["some_future_field"]["nested"], true);
    }
}
