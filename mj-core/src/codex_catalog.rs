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

/// Parse a provider's `GET /models` response body.
///
/// Two shapes are accepted. The Codex shape, `{"models": [...]}`, is what Z.ai
/// serves and is kept entry for entry. OpenAI's plain model list,
/// `{"object": "list", "data": [{"id": "..."}]}`, is what DeepSeek and most
/// OpenAI-compatible providers serve; it carries only ids, so each entry is
/// translated into a full Codex entry using the conservative defaults in
/// [`entry_from_openai_model`]. A body with neither array is an error naming
/// both shapes.
pub fn parse(bytes: &[u8]) -> Result<CodexCatalog> {
    let value: Value = serde_json::from_slice(bytes).context("parse model catalog as JSON")?;
    let entries = if let Some(models) = value.get("models").and_then(Value::as_array) {
        let mut entries = Vec::with_capacity(models.len());
        for model in models {
            let Some(object) = model.as_object() else {
                bail!("model catalog entry is not a JSON object");
            };
            entries.push(object.clone());
        }
        entries
    } else if let Some(data) = value.get("data").and_then(Value::as_array) {
        let mut entries = Vec::with_capacity(data.len());
        for (position, model) in data.iter().enumerate() {
            let Some(object) = model.as_object() else {
                bail!("model list entry is not a JSON object");
            };
            let Some(id) = object.get("id").and_then(Value::as_str) else {
                bail!("model list entry has no `id`");
            };
            entries.push(entry_from_openai_model(
                id,
                object.get("owned_by").and_then(Value::as_str),
                position,
            ));
        }
        entries
    } else {
        bail!(
            "model catalog has neither a `models` array (Codex catalog format) nor a `data` array (OpenAI model list format)"
        );
    };
    if entries.is_empty() {
        bail!("model catalog lists no models");
    }
    Ok(CodexCatalog { models: entries })
}

/// Parse a Codex-shape catalog only, for the user's optional `models.json`
/// override file. The override file refines fetched entries, so it must speak
/// the same language as the catalog it refines.
pub fn parse_codex_shape(bytes: &[u8]) -> Result<CodexCatalog> {
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

/// Build a Codex catalog entry from one id in an OpenAI-format model list.
///
/// The list carries no capabilities, so the defaults are deliberately
/// conservative: no reasoning levels (a session shows no effort choice rather
/// than offering one the provider rejects), a 128k context window, and the
/// plain shell tool. A user who knows better refines any entry with a
/// `models.json` override file in the profile home.
fn entry_from_openai_model(
    id: &str,
    owned_by: Option<&str>,
    position: usize,
) -> Map<String, Value> {
    let description = match owned_by {
        Some(owner) if !owner.is_empty() => format!("{id} ({owner})"),
        _ => id.to_owned(),
    };
    let mut entry = Map::new();
    entry.insert("slug".to_owned(), Value::from(id));
    entry.insert("display_name".to_owned(), Value::from(id));
    entry.insert("description".to_owned(), Value::from(description));
    entry.insert(
        "supported_reasoning_levels".to_owned(),
        Value::Array(vec![]),
    );
    entry.insert("shell_type".to_owned(), Value::from("shell_command"));
    entry.insert("visibility".to_owned(), Value::from("list"));
    entry.insert("supported_in_api".to_owned(), Value::Bool(true));
    entry.insert("priority".to_owned(), Value::from(position as u64));
    entry.insert("base_instructions".to_owned(), Value::from(""));
    entry.insert(
        "supports_reasoning_summaries".to_owned(),
        Value::Bool(false),
    );
    entry.insert("default_reasoning_summary".to_owned(), Value::from("none"));
    entry.insert("support_verbosity".to_owned(), Value::Bool(false));
    entry.insert("apply_patch_tool_type".to_owned(), Value::from("freeform"));
    entry.insert(
        "truncation_policy".to_owned(),
        serde_json::json!({"mode": "bytes", "limit": 10000}),
    );
    entry.insert("context_window".to_owned(), Value::from(128000));
    entry.insert("max_context_window".to_owned(), Value::from(128000));
    entry.insert(
        "effective_context_window_percent".to_owned(),
        Value::from(95),
    );
    entry.insert("supports_parallel_tool_calls".to_owned(), Value::Bool(true));
    entry.insert(
        "experimental_supported_tools".to_owned(),
        Value::Array(vec![]),
    );
    entry.insert(
        "input_modalities".to_owned(),
        Value::Array(vec![Value::from("text")]),
    );
    entry
}

/// Copy the user's override entries over the fetched catalog.
///
/// An override entry whose `slug` the provider listed replaces those fields on
/// the fetched entry and leaves the rest; a slug the provider did not list is
/// appended, so a user can advertise a model the provider's list omits. This is
/// how a user adds reasoning levels or a larger context window to a provider
/// whose model list carries only ids.
pub fn merge_overrides(catalog: &mut CodexCatalog, overrides: &CodexCatalog) {
    for entry in &overrides.models {
        let Some(slug) = entry.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let existing = catalog
            .models
            .iter_mut()
            .find(|model| model.get("slug").and_then(Value::as_str) == Some(slug));
        match existing {
            Some(existing) => {
                for (field, value) in entry {
                    existing.insert(field.clone(), value.clone());
                }
            }
            None => catalog.models.push(entry.clone()),
        }
    }
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

    const OPENAI_LIST: &[u8] = br#"{"object":"list","data":[
        {"id":"deepseek-flash","object":"model","owned_by":"deepseek"},
        {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
    ]}"#;

    #[test]
    fn parse_translates_an_openai_model_list_into_catalog_entries() {
        let catalog = parse(OPENAI_LIST).expect("parse");
        assert_eq!(catalog.slugs(), ["deepseek-flash", "deepseek-v4-pro"]);
        let flash = &catalog.models[0];
        assert_eq!(flash["display_name"], Value::from("deepseek-flash"));
        assert_eq!(
            flash["description"],
            Value::from("deepseek-flash (deepseek)"),
            "the owner explains where the model came from"
        );
        assert_eq!(
            flash["supported_reasoning_levels"],
            Value::Array(vec![]),
            "a plain list says nothing about reasoning, so no effort is offered"
        );
        assert_eq!(flash["context_window"], Value::from(128000));
        assert_eq!(flash["priority"], Value::from(0));
        assert_eq!(catalog.models[1]["priority"], Value::from(1));
        assert_eq!(flash["truncation_policy"]["limit"], Value::from(10000));
        // Codex must be able to read back what the translation produced.
        assert!(parse(catalog.to_json().as_bytes()).is_ok());
    }

    #[test]
    fn parse_rejects_a_body_in_neither_shape_and_names_both() {
        let error = parse(br#"{"available":["deepseek-v4-pro"]}"#)
            .expect_err("a third shape cannot be guessed at")
            .to_string();
        assert!(error.contains("models"), "{error}");
        assert!(error.contains("data"), "{error}");
    }

    #[test]
    fn overrides_refine_a_listed_model_and_append_an_unlisted_one() {
        let mut catalog = parse(OPENAI_LIST).expect("parse");
        let overrides = parse_codex_shape(
            br#"{"models":[
                {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high"],"context_window":256000},
                {"slug":"deepseek-reasoner","display_name":"DeepSeek Reasoner"}
            ]}"#,
        )
        .expect("parse overrides");

        merge_overrides(&mut catalog, &overrides);

        assert_eq!(
            catalog.slugs(),
            ["deepseek-flash", "deepseek-v4-pro", "deepseek-reasoner"],
            "an unlisted slug is appended in override order"
        );
        let pro = &catalog.models[1];
        assert_eq!(
            pro["supported_reasoning_levels"],
            serde_json::json!(["low", "high"]),
            "the override field replaces the translated default"
        );
        assert_eq!(pro["context_window"], Value::from(256000));
        assert_eq!(
            pro["display_name"],
            Value::from("deepseek-v4-pro"),
            "fields the override omits survive"
        );
        assert_eq!(
            catalog.models[0]["supported_reasoning_levels"],
            Value::Array(vec![]),
            "a model the override does not name is untouched"
        );
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
