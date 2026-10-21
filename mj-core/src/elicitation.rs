//! ACP form elicitation shapes shared by the runtime, relay, and chat UI.
//!
//! The ACP crate version Hel currently uses predates enum-option descriptions,
//! so requests are parsed from their raw JSON here. Keeping that interpretation
//! in one place also lets Claude and Codex metadata conventions share one UI.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_ELICITATION_BYTES: usize = 64 * 1024;
const MAX_FIELDS: usize = 32;
const MAX_OPTIONS: usize = 100;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationRequest {
    pub id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<ElicitationField>,
    /// Set when the form shows a tool permission request. Its Skip rejects
    /// only that tool call and its Cancel stops the agent's turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_permission: Option<ToolPermissionForm>,
}

/// What a tool permission form offers beyond the request's own options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPermissionForm {
    /// Whether the request has a one-time rejection for Skip to select. A
    /// request without one offers no Skip.
    pub skip: bool,
}

impl ElicitationRequest {
    /// Whether the form offers Skip (the `decline` action). Every surface
    /// asks this, and [`Self::harness_response`] refuses a Skip it denies.
    #[must_use]
    pub fn offers_skip(&self) -> bool {
        self.tool_permission.is_none_or(|form| form.skip)
    }

    pub fn from_acp_params(id: impl Into<String>, params: Value) -> Result<Self> {
        let encoded = serde_json::to_vec(&params).context("serialize ACP elicitation request")?;
        ensure!(
            encoded.len() <= MAX_ELICITATION_BYTES,
            "elicitation request exceeds {MAX_ELICITATION_BYTES} bytes"
        );
        let params = object(&params, "elicitation params")?;
        ensure!(
            string(params, "mode")? == "form",
            "Mjolnir only supports form elicitations"
        );
        ensure!(
            params.get("sessionId").and_then(Value::as_str).is_some()
                || params.get("requestId").is_some(),
            "elicitation has no sessionId or requestId scope"
        );
        let message = string(params, "message")?.to_owned();
        ensure_text(&message, "elicitation message")?;
        let schema = object(
            params
                .get("requestedSchema")
                .context("form elicitation has no requestedSchema")?,
            "requestedSchema",
        )?;
        if let Some(kind) = schema.get("type").and_then(Value::as_str) {
            ensure!(kind == "object", "requestedSchema type must be object");
        }
        let title = optional_text(schema, "title")?;
        let description = optional_text(schema, "description")?;
        let properties = schema
            .get("properties")
            .map(|value| object(value, "requestedSchema.properties"))
            .transpose()?
            .cloned()
            .unwrap_or_default();
        ensure!(
            properties.len() <= MAX_FIELDS,
            "elicitation has more than {MAX_FIELDS} fields"
        );
        let required = string_set(schema.get("required"), "requestedSchema.required")?;
        let mut fields = properties
            .iter()
            .map(|(field_id, schema)| parse_field(field_id, schema, required.contains(field_id)))
            .collect::<Result<Vec<_>>>()?;
        title_untitled_question_from_message(&message, &mut fields);
        Ok(Self {
            id: id.into(),
            message,
            title,
            description,
            fields,
            tool_permission: None,
        })
    }

    /// Whether a submitted answer satisfies this request's own schema.
    pub fn validate_response(&self, response: &ElicitationResponse) -> Result<(), String> {
        self.harness_response(response.clone()).map(drop)
    }

    /// The answer the harness receives for a submitted one, or why the
    /// submitted answer does not fit this request.
    ///
    /// Every surface (terminal, web, `mj respond`) submits each field's own
    /// value: a select's picks, and the text of its companion field. How the
    /// two combine depends on the companion's [`CompanionRole`]:
    ///
    /// - [`CompanionRole::Alongside`] (claude-agent-acp's "Other") goes beside
    ///   any pick: the bridge joins a multi-select's picks with the text and
    ///   attaches the text to a single pick as a note, so both are forwarded.
    /// - [`CompanionRole::Alternative`] (codex-acp's note) is the answer in
    ///   place of a pick. Its text becomes the select's own value, the Air
    ///   convention codex-acp reads on both its question paths
    ///   (`typedChoiceAnswer` and the async answer), and a pick sent with it
    ///   is refused: codex-acp's async path would read the note in place of
    ///   the pick.
    /// - [`CompanionRole::Detail`] (plan-review feedback on Revise) goes with
    ///   its option.
    ///
    /// A required select answered only by its companion gets the text as its
    /// own value, so the harness never receives an answer that omits a
    /// required field. Only that move puts text outside the offered options
    /// into a select; a submitted select value must still be one of them.
    ///
    /// Surfaces that answer out of band — the phone posts an answer over
    /// HTTP — can send content for a request the agent has already replaced,
    /// or content the agent never offered. Checking the answer against the
    /// fields this request actually published means a stale or malformed
    /// payload is refused instead of being forwarded as something the
    /// harness never asked for.
    pub fn harness_response(
        &self,
        response: ElicitationResponse,
    ) -> Result<ElicitationResponse, String> {
        let ElicitationResponse::Accept { mut content } = response else {
            if response == ElicitationResponse::Decline && !self.offers_skip() {
                return Err(
                    "this permission request offers no one-time rejection to skip with; \
                     choose one of its options, or cancel to stop the turn"
                        .to_owned(),
                );
            }
            return Ok(response);
        };
        for (id, value) in &content {
            let Some(field) = self.fields.iter().find(|field| field.id == *id) else {
                return Err(format!("{id:?} is not a field of this request"));
            };
            validate_field_value(field, value)?;
        }
        for field in &self.fields {
            let Some((companion, role)) = self.companion(field) else {
                continue;
            };
            let companion = &self.fields[companion];
            let Some(ElicitationValue::String(text)) = content.get(&companion.id) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            if content.contains_key(&field.id) {
                if role == CompanionRole::Alternative {
                    return Err(format!(
                        "{} takes either an offered option or {}, not both",
                        field.title, companion.title
                    ));
                }
                continue;
            }
            // Detail belongs to its option and never answers the select; text
            // beside a pick answers an optional select as it is.
            match role {
                CompanionRole::Detail => continue,
                CompanionRole::Alongside if !field.required => continue,
                CompanionRole::Alongside | CompanionRole::Alternative => {}
            }
            let Some(ElicitationValue::String(text)) = content.remove(&companion.id) else {
                unreachable!("the companion answer was just read as text")
            };
            let value = match field.kind {
                ElicitationFieldKind::MultiSelect { .. } => {
                    ElicitationValue::StringArray(vec![text])
                }
                _ => ElicitationValue::String(text),
            };
            content.insert(field.id.clone(), value);
        }
        for field in self.fields.iter().filter(|field| field.required) {
            match content.get(&field.id) {
                None => return Err(format!("{} is required", field.title)),
                Some(ElicitationValue::String(text)) if text.trim().is_empty() => {
                    return Err(format!("{} is required", field.title));
                }
                Some(_) => {}
            }
        }
        Ok(ElicitationResponse::Accept { content })
    }

    /// `select`'s companion text field and its role: the field whose
    /// `custom_answer_for` names `select`, as its index in `fields`. The first
    /// one counts. This is the one rule every Rust surface lays out by; the
    /// web viewer's `buildElicitationForm` mirrors it.
    #[must_use]
    pub fn companion(&self, select: &ElicitationField) -> Option<(usize, CompanionRole)> {
        if !matches!(
            select.kind,
            ElicitationFieldKind::SingleSelect { .. } | ElicitationFieldKind::MultiSelect { .. }
        ) {
            return None;
        }
        self.fields
            .iter()
            .position(|field| {
                field.custom_answer_for.as_deref() == Some(select.id.as_str())
                    && matches!(field.kind, ElicitationFieldKind::Text { .. })
            })
            .map(|index| (index, self.fields[index].companion_role()))
    }

    /// The user's reply for conversation history. Mask secrets here, before
    /// the runtime event or durable journal can retain them.
    pub fn reply_text(&self, response: &ElicitationResponse) -> String {
        let mut lines = vec![
            self.title.as_ref().unwrap_or(&self.message).clone(),
            String::new(),
        ];
        match response {
            ElicitationResponse::Accept { content } => {
                for field in &self.fields {
                    let Some(value) = content.get(&field.id) else {
                        continue;
                    };
                    let parent = field
                        .custom_answer_for
                        .as_ref()
                        .and_then(|id| self.fields.iter().find(|parent| parent.id == *id));
                    let title = parent
                        .filter(|parent| !content.contains_key(&parent.id))
                        .map_or(&field.title, |parent| &parent.title);
                    let text = if field.secret || parent.is_some_and(|parent| parent.secret) {
                        "[hidden]".to_owned()
                    } else {
                        field.reply_value_text(value)
                    };
                    lines.push(format!("{title}: {text}"));
                }
                if lines.len() == 2 {
                    lines.push("Accepted".to_owned());
                }
            }
            ElicitationResponse::Decline => lines.push("Skipped".to_owned()),
            ElicitationResponse::Cancel => lines.push("Cancelled".to_owned()),
        }
        lines.join("\n")
    }
}

/// Whether one answered value satisfies the constraints of the field it
/// answers. Presence and required-ness are the caller's business: this checks
/// only that the value the caller does have is one the schema allows.
pub fn validate_field_value(
    field: &ElicitationField,
    value: &ElicitationValue,
) -> Result<(), String> {
    match (&field.kind, value) {
        (
            ElicitationFieldKind::Text {
                min_length,
                max_length,
                pattern,
                format,
                ..
            },
            ElicitationValue::String(value),
        ) => {
            let length = value.chars().count();
            if min_length.is_some_and(|minimum| length < minimum) {
                return Err(format!("{} is too short", field.title));
            }
            if max_length.is_some_and(|maximum| length > maximum) {
                return Err(format!("{} is too long", field.title));
            }
            if pattern.as_ref().is_some_and(|pattern| {
                !regex::Regex::new(pattern)
                    .expect("validated pattern")
                    .is_match(value)
            }) {
                return Err(format!(
                    "{} does not match the required format",
                    field.title
                ));
            }
            if let Some(format) = format {
                validate_text_format(value, format)
                    .map_err(|message| format!("{} {message}", field.title))?;
            }
            Ok(())
        }
        (ElicitationFieldKind::SingleSelect { options, .. }, ElicitationValue::String(value)) => {
            if options.iter().any(|option| option.value == *value) {
                Ok(())
            } else {
                Err(format!("{} is not one of the offered options", field.title))
            }
        }
        (
            ElicitationFieldKind::MultiSelect {
                options,
                min_items,
                max_items,
                ..
            },
            ElicitationValue::StringArray(values),
        ) => {
            if min_items.is_some_and(|minimum| values.len() < minimum) {
                return Err(format!("{} needs more selections", field.title));
            }
            if max_items.is_some_and(|maximum| values.len() > maximum) {
                return Err(format!("{} has too many selections", field.title));
            }
            if values
                .iter()
                .all(|value| options.iter().any(|option| option.value == *value))
            {
                Ok(())
            } else {
                Err(format!("{} is not one of the offered options", field.title))
            }
        }
        (ElicitationFieldKind::Boolean { .. }, ElicitationValue::Boolean(_)) => Ok(()),
        (
            ElicitationFieldKind::Integer {
                minimum, maximum, ..
            },
            ElicitationValue::Integer(value),
        ) => {
            if minimum.is_some_and(|minimum| *value < minimum)
                || maximum.is_some_and(|maximum| *value > maximum)
            {
                return Err(format!("{} is outside the allowed range", field.title));
            }
            Ok(())
        }
        (
            ElicitationFieldKind::Number {
                minimum, maximum, ..
            },
            value @ (ElicitationValue::Number(_) | ElicitationValue::Integer(_)),
        ) => {
            // JSON carries a whole number for a `number` field as an integer,
            // and an untagged decode reports it as one, so both arrive here.
            #[allow(clippy::cast_precision_loss)]
            let value = match value {
                ElicitationValue::Number(value) => *value,
                ElicitationValue::Integer(value) => *value as f64,
                _ => unreachable!("matched number or integer"),
            };
            if !value.is_finite()
                || minimum.is_some_and(|minimum| value < minimum)
                || maximum.is_some_and(|maximum| value > maximum)
            {
                return Err(format!("{} is outside the allowed range", field.title));
            }
            Ok(())
        }
        _ => Err(format!("{} has an incompatible value", field.title)),
    }
}

/// String formats the ACP schema can request. Unknown formats are accepted:
/// the schema keyword is open-ended and refusing an answer over a format Hel
/// does not model would block a question the agent can otherwise use.
fn validate_text_format(value: &str, format: &str) -> Result<(), &'static str> {
    match format {
        "email"
            if !value.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
            }) =>
        {
            Err("must be an email address")
        }
        "uri" if url::Url::parse(value).is_err() => Err("must be a URI"),
        "date" if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() => {
            Err("must be a YYYY-MM-DD date")
        }
        "date-time" if chrono::DateTime::parse_from_rfc3339(value).is_err() => {
            Err("must be an RFC 3339 date-time")
        }
        _ => Ok(()),
    }
}

/// The `custom_answer_option` of a companion that goes beside any pick of its
/// select, or answers on its own when nothing is picked.
///
/// It is not an option value. It rides in the existing field so the relayed,
/// journaled and stored form keeps its shape: a reader from before it finds
/// no option of that value and treats the companion as the select's
/// free-text alternative, the earlier behavior, which the bridge also reads.
pub const COMPANION_ALONGSIDE: &str = "\u{1f}alongside";

/// How a select's companion text field (the field whose `custom_answer_for`
/// names the select) takes part in the select's answer. The parser decides it
/// from the bridge's own marker on the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionRole {
    /// The answer in place of any pick: codex-acp's `user_note`, which its
    /// async question path reads instead of the pick.
    Alternative,
    /// Beside any pick, or the whole answer when nothing is picked:
    /// claude-agent-acp's "Other" ("Type your own answer, or add a note to
    /// the option you chose above").
    Alongside,
    /// The detail of the one option `custom_answer_option` names, sent with
    /// it: Mjolnir's plan-review feedback on Revise.
    Detail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationField {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_answer_for: Option<String>,
    /// For a companion: the option its text belongs to, or
    /// [`COMPANION_ALONGSIDE`]. See [`ElicitationField::companion_role`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_answer_option: Option<String>,
    #[serde(flatten)]
    pub kind: ElicitationFieldKind,
}

impl ElicitationField {
    /// The role this field plays as a companion. Only meaningful for a text
    /// field whose `custom_answer_for` names a select.
    #[must_use]
    pub fn companion_role(&self) -> CompanionRole {
        match self.custom_answer_option.as_deref() {
            None => CompanionRole::Alternative,
            Some(COMPANION_ALONGSIDE) => CompanionRole::Alongside,
            Some(_) => CompanionRole::Detail,
        }
    }

    fn reply_value_text(&self, value: &ElicitationValue) -> String {
        let option_title = |value: &str| match &self.kind {
            ElicitationFieldKind::SingleSelect { options, .. }
            | ElicitationFieldKind::MultiSelect { options, .. } => options
                .iter()
                .find(|option| option.value == value)
                .map_or(value, |option| option.title.as_str())
                .to_owned(),
            _ => value.to_owned(),
        };
        match value {
            ElicitationValue::String(value) => option_title(value),
            ElicitationValue::StringArray(values) => values
                .iter()
                .map(|value| option_title(value))
                .collect::<Vec<_>>()
                .join(", "),
            ElicitationValue::Integer(value) => value.to_string(),
            ElicitationValue::Number(value) => value.to_string(),
            ElicitationValue::Boolean(value) => if *value { "Yes" } else { "No" }.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElicitationFieldKind {
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    SingleSelect {
        options: Vec<ElicitationOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    MultiSelect {
        options: Vec<ElicitationOption>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_items: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_items: Option<usize>,
    },
    Boolean {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Integer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<i64>,
    },
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationOption {
    pub value: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElicitationResponse {
    Accept {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        content: BTreeMap<String, ElicitationValue>,
    },
    Decline,
    Cancel,
}

impl ElicitationResponse {
    pub const fn action_name(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ElicitationValue {
    String(String),
    Integer(i64),
    Number(f64),
    Boolean(bool),
    StringArray(Vec<String>),
}

/// Muse sends one question per form, names its field `q0`, and puts the
/// question in the message as "Header: question". Claude puts the header in
/// the field title, so a form titled with its own id borrows the header from
/// the message, and the form reads the same for both.
fn title_untitled_question_from_message(message: &str, fields: &mut [ElicitationField]) {
    let [field] = fields else {
        return;
    };
    if field.title != field.id {
        return;
    }
    if let Some((header, _)) = message.split_once(": ")
        && !header.trim().is_empty()
        && header.chars().count() <= 40
        && !header.contains('\n')
    {
        field.title = header.trim().to_owned();
    }
}

fn parse_field(field_id: &str, value: &Value, required: bool) -> Result<ElicitationField> {
    ensure_text(field_id, "elicitation field id")?;
    let schema = object(value, &format!("schema for field {field_id:?}"))?;
    let title = optional_text(schema, "title")?.unwrap_or_else(|| field_id.to_owned());
    let description = optional_text(schema, "description")?;
    let meta = schema.get("_meta").and_then(Value::as_object);
    let codex = meta
        .and_then(|meta| meta.get("codex"))
        .and_then(Value::as_object);
    let secret = codex
        .and_then(|meta| meta.get("isSecret"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A companion names its select with the bridge's own marker, and the
    // marker also says how its text relates to the pick. codex-acp's note
    // (`_meta.codex`, `role: "user_note"`) is the answer in place of a pick;
    // claude-agent-acp's "Other" (the Air `customAnswer` key, sent because
    // Mjolnir declares `_meta.jetbrains.air`) goes beside it.
    let air_custom_answer = meta
        .and_then(|meta| meta.get("jetbrains"))
        .and_then(|jetbrains| jetbrains.get("air"))
        .and_then(|air| air.get("customAnswer"))
        .and_then(Value::as_object);
    let codex_note_for = codex
        .filter(|meta| meta.get("role").and_then(Value::as_str) == Some("user_note"))
        .and_then(|meta| meta.get("questionId"))
        .and_then(Value::as_str);
    let claude_other_for = air_custom_answer
        .filter(|meta| meta.get("isCustomAnswer").and_then(Value::as_bool) == Some(true))
        .and_then(|meta| meta.get("questionId"))
        .and_then(Value::as_str);
    let (custom_answer_for, custom_answer_option) = match (codex_note_for, claude_other_for) {
        (Some(question), _) => (Some(question.to_owned()), None),
        (None, Some(question)) => (
            Some(question.to_owned()),
            Some(COMPANION_ALONGSIDE.to_owned()),
        ),
        (None, None) => (None, None),
    };
    let kind = match string(schema, "type")? {
        "string" => {
            let options = parse_string_options(schema)?;
            if options.is_empty() {
                let pattern = optional_text(schema, "pattern")?;
                if let Some(pattern) = &pattern {
                    regex::Regex::new(pattern)
                        .with_context(|| format!("field {field_id:?} has an invalid pattern"))?;
                }
                ElicitationFieldKind::Text {
                    default: optional_text(schema, "default")?,
                    min_length: optional_usize(schema, "minLength")?,
                    max_length: optional_usize(schema, "maxLength")?,
                    pattern,
                    format: optional_text(schema, "format")?,
                }
            } else {
                ElicitationFieldKind::SingleSelect {
                    options,
                    default: optional_text(schema, "default")?,
                }
            }
        }
        "array" => {
            let items = object(
                schema
                    .get("items")
                    .context("multi-select field has no items schema")?,
                "multi-select items",
            )?;
            let options = parse_options(items, "anyOf").or_else(|_| parse_enum_options(items))?;
            ensure!(!options.is_empty(), "multi-select field has no options");
            ElicitationFieldKind::MultiSelect {
                options,
                default: optional_string_array(schema, "default")?.unwrap_or_default(),
                min_items: optional_usize(schema, "minItems")?,
                max_items: optional_usize(schema, "maxItems")?,
            }
        }
        "boolean" => ElicitationFieldKind::Boolean {
            default: optional_bool(schema, "default")?,
        },
        "integer" => ElicitationFieldKind::Integer {
            default: optional_i64(schema, "default")?,
            minimum: optional_i64(schema, "minimum")?,
            maximum: optional_i64(schema, "maximum")?,
        },
        "number" => ElicitationFieldKind::Number {
            default: optional_f64(schema, "default")?,
            minimum: optional_f64(schema, "minimum")?,
            maximum: optional_f64(schema, "maximum")?,
        },
        other => bail!("field {field_id:?} has unsupported type {other:?}"),
    };
    Ok(ElicitationField {
        id: field_id.to_owned(),
        title,
        description,
        required,
        secret,
        custom_answer_for,
        custom_answer_option,
        kind,
    })
}

fn parse_string_options(schema: &Map<String, Value>) -> Result<Vec<ElicitationOption>> {
    if schema.contains_key("oneOf") {
        parse_options(schema, "oneOf")
    } else if schema.contains_key("enum") {
        parse_enum_options(schema)
    } else {
        Ok(Vec::new())
    }
}

fn parse_options(schema: &Map<String, Value>, key: &str) -> Result<Vec<ElicitationOption>> {
    let values = schema
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("{key} must be an array"))?;
    ensure!(values.len() <= MAX_OPTIONS, "{key} has too many options");
    values
        .iter()
        .map(|value| {
            let option = object(value, "elicitation option")?;
            let value = string(option, "const")?.to_owned();
            let title = optional_text(option, "title")?.unwrap_or_else(|| value.clone());
            let description = optional_text(option, "description")?;
            let preview = option
                .get("_meta")
                .and_then(Value::as_object)
                .and_then(|meta| meta.get("_claude/askUserQuestionOption"))
                .and_then(Value::as_object)
                .and_then(|meta| meta.get("preview"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(preview) = &preview {
                ensure_text(preview, "elicitation option preview")?;
            }
            Ok(ElicitationOption {
                value,
                title,
                description,
                preview,
            })
        })
        .collect()
}

fn parse_enum_options(schema: &Map<String, Value>) -> Result<Vec<ElicitationOption>> {
    let values = schema
        .get("enum")
        .and_then(Value::as_array)
        .context("enum must be an array")?;
    ensure!(values.len() <= MAX_OPTIONS, "enum has too many options");
    values
        .iter()
        .map(|value| {
            let value = value
                .as_str()
                .context("elicitation enum values must be strings")?
                .to_owned();
            Ok(ElicitationOption {
                title: value.clone(),
                value,
                description: None,
                preview: None,
            })
        })
        .collect()
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{what} must be an object"))
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))
}

fn ensure_text(value: &str, what: &str) -> Result<()> {
    ensure!(value.len() <= MAX_TEXT_BYTES, "{what} is too long");
    Ok(())
}

fn optional_text(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            ensure_text(value, key)?;
            Ok(Some(value.clone()))
        }
        Some(_) => bail!("{key} must be a string"),
    }
}

fn optional_usize(object: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    optional_u64(object, key)?
        .map(|value| usize::try_from(value).context("numeric constraint does not fit usize"))
        .transpose()
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .with_context(|| format!("{key} must be a non-negative integer"))
            .map(Some),
    }
}

fn optional_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .with_context(|| format!("{key} must be an integer"))
            .map(Some),
    }
}

fn optional_f64(object: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value
                .as_f64()
                .with_context(|| format!("{key} must be a number"))?;
            ensure!(value.is_finite(), "{key} must be finite");
            Ok(Some(value))
        }
    }
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .with_context(|| format!("{key} must be a boolean"))
            .map(Some),
    }
}

fn optional_string_array(object: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let values = value
        .as_array()
        .with_context(|| format!("{key} must be an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{key} entries must be strings"))
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn string_set(value: Option<&Value>, what: &str) -> Result<BTreeSet<String>> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    value
        .as_array()
        .with_context(|| format!("{what} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{what} entries must be strings"))
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Hard-won: dc436f0c: Claude bridge 0.84 moved Other-answer fields and Mjolnir lost their pairing
    #[test]
    fn parses_claude_choices_preview_and_custom_answer_marker() {
        let request = ElicitationRequest::from_acp_params(
            "elicit-1",
            json!({
                "sessionId": "session-1",
                "toolCallId": "tool-1",
                "mode": "form",
                "message": "Which architecture?",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "question_0": {
                            "type": "string",
                            "title": "CI architecture",
                            "oneOf": [{
                                "const": "reusable",
                                "title": "Reusable workflow",
                                "description": "Thin callers",
                                "_meta": {"_claude/askUserQuestionOption": {"preview": "ci.yml -> reusable"}}
                            }]
                        },
                        // claude-agent-acp 0.90.0 marks the companion of an
                        // Air client under both keys; Mjolnir reads the Air one.
                        "question_0_custom": {
                            "type": "string",
                            "title": "Other",
                            "_meta": {
                                "_askUserQuestionCustomAnswer": {
                                    "questionId": "question_0",
                                    "isCustomAnswer": true
                                },
                                "jetbrains": {"air": {"version": 1, "customAnswer": {
                                    "questionId": "question_0",
                                    "isCustomAnswer": true
                                }}}
                            }
                        }
                    }
                }
            }),
        )
        .unwrap();
        assert_eq!(request.message, "Which architecture?");
        let ElicitationFieldKind::SingleSelect { options, .. } = &request.fields[0].kind else {
            panic!("expected single-select field");
        };
        assert_eq!(options[0].description.as_deref(), Some("Thin callers"));
        assert_eq!(options[0].preview.as_deref(), Some("ci.yml -> reusable"));
        assert_eq!(
            request.fields[1].custom_answer_for.as_deref(),
            Some("question_0")
        );
        // Claude's "Other" goes beside a pick, or answers on its own.
        assert_eq!(request.fields[1].companion_role(), CompanionRole::Alongside);
    }

    // Hard-won: 86ab9abd: Muse forms showed q0 and Sessions rows showed the tool name
    #[test]
    fn an_untitled_single_question_takes_the_header_from_its_message() {
        let request = |message: &str, title: Option<&str>| {
            let mut field = json!({
                "type": "string",
                "oneOf": [{"const": "Red", "title": "Red"}]
            });
            if let Some(title) = title {
                field["title"] = json!(title);
            }
            ElicitationRequest::from_acp_params(
                "elicit-muse",
                json!({
                    "sessionId": "session-1",
                    "mode": "form",
                    "message": message,
                    "requestedSchema": {"type": "object", "properties": {"q0": field}}
                }),
            )
            .unwrap()
        };
        let muse = request("Color: Which colour do you prefer?", Some("q0"));
        assert_eq!(muse.fields[0].title, "Color");
        assert_eq!(muse.fields[0].id, "q0", "the answer is still keyed by id");
        assert_eq!(request("Color: Which?", None).fields[0].title, "Color");
        // A real title, or a message with no header, is left alone.
        assert_eq!(request("Color: Which?", Some("Hue")).fields[0].title, "Hue");
        assert_eq!(request("Which colour?", None).fields[0].title, "q0");
    }

    #[test]
    fn parses_codex_secret_other_field() {
        let request = ElicitationRequest::from_acp_params(
            "elicit-2",
            json!({
                "sessionId": "session-1",
                "mode": "form",
                "message": "Input requested",
                "requestedSchema": {
                    "properties": {
                        "token__other": {
                            "type": "string",
                            "_meta": {"codex": {
                                "questionId": "token",
                                "role": "user_note",
                                "isSecret": true
                            }}
                        }
                    }
                }
            }),
        )
        .unwrap();
        assert!(request.fields[0].secret);
        assert_eq!(
            request.fields[0].custom_answer_for.as_deref(),
            Some("token")
        );
    }

    // Hard-won: #1312: a required Codex question answered in its note reached the harness without its own field
    #[test]
    fn pairs_codex_user_note_with_its_question() {
        // The form @brokkai/codex-acp 1.13.7 sends an Air client for a
        // question that accepts an answer outside its listed options: no
        // "None of the above" choice, and Air markers on the note.
        let request = ElicitationRequest::from_acp_params(
            "elicit-note",
            json!({
                "sessionId": "session-1",
                "mode": "form",
                "message": "Codex needs your input to continue.",
                "requestedSchema": {
                    "type": "object",
                    "required": ["scope"],
                    "properties": {
                        "scope": {
                            "type": "string",
                            "title": "Which scope?",
                            "description": "Scope",
                            "oneOf": [
                                {"const": "Narrow", "title": "Narrow"},
                                {"const": "Wide", "title": "Wide"}
                            ],
                            "_meta": {"codex": {"isOther": true, "isSecret": false}}
                        },
                        "scope_note": {
                            "type": "string",
                            "title": "Additional answer or note",
                            "_meta": {
                                "codex": {
                                    "questionId": "scope",
                                    "role": "user_note",
                                    "isSecret": false
                                },
                                "_askUserQuestionCustomAnswer": true,
                                "jetbrains": {"air": {"version": 1, "customAnswer": true}}
                            }
                        }
                    }
                }
            }),
        )
        .unwrap();
        let note = request
            .fields
            .iter()
            .find(|field| field.id == "scope_note")
            .unwrap();
        assert_eq!(note.custom_answer_for.as_deref(), Some("scope"));
        assert_eq!(note.custom_answer_option, None);
        let answer = |pairs: &[(&str, &str)]| ElicitationResponse::Accept {
            content: pairs
                .iter()
                .map(|(id, value)| {
                    (
                        (*id).to_owned(),
                        ElicitationValue::String((*value).to_owned()),
                    )
                })
                .collect(),
        };
        // An answer written in the note is the question's own answer. The
        // harness gets it in the question field, where codex-acp reads an Air
        // client's typed answer, so the required field is never missing.
        assert_eq!(
            request.harness_response(answer(&[("scope_note", "Only the parser")])),
            Ok(answer(&[("scope", "Only the parser")]))
        );
        // The note is an alternative to a pick, never beside one: codex-acp's
        // async question path reads a note in place of the pick, so sending
        // both would lose the pick.
        assert_eq!(note.companion_role(), CompanionRole::Alternative);
        assert!(
            request
                .validate_response(&answer(&[
                    ("scope", "Narrow"),
                    ("scope_note", "Parser first")
                ]))
                .is_err()
        );
        // Neither a pick nor text leaves the required question unanswered,
        // and a submitted pick must still be an offered option.
        assert!(
            request
                .validate_response(&answer(&[("scope_note", "  ")]))
                .is_err()
        );
        assert!(
            request
                .validate_response(&answer(&[("scope", "Only the parser")]))
                .is_err()
        );
    }

    // Hard-won: #1312: plan-review feedback alone was forwarded as the plan decision
    #[test]
    fn plan_review_feedback_never_stands_in_for_its_decision() {
        use crate::acp::{PLAN_REVIEW_ACTION, PLAN_REVIEW_FEEDBACK, normalized_plan_review};
        let review = normalized_plan_review("plan-review-1".into(), &json!({"plan": "1. Do it"}));
        let answer = |pairs: &[(&str, &str)]| ElicitationResponse::Accept {
            content: pairs
                .iter()
                .map(|(id, value)| {
                    (
                        (*id).to_owned(),
                        ElicitationValue::String((*value).to_owned()),
                    )
                })
                .collect(),
        };
        assert_eq!(
            review.harness_response(answer(&[(PLAN_REVIEW_FEEDBACK, "tighten step 3")])),
            Err("Decision is required".to_owned())
        );
        let revise = answer(&[
            (PLAN_REVIEW_ACTION, "revise"),
            (PLAN_REVIEW_FEEDBACK, "tighten step 3"),
        ]);
        assert_eq!(review.harness_response(revise.clone()), Ok(revise));
    }

    fn numeric_request() -> ElicitationRequest {
        ElicitationRequest::from_acp_params(
            "elicit-3",
            json!({
                "sessionId": "session-1",
                "mode": "form",
                "message": "Size the run",
                "requestedSchema": {
                    "type": "object",
                    "required": ["workers"],
                    "properties": {
                        "workers": {"type": "integer", "minimum": 1, "maximum": 8},
                        "ratio": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "targets": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 2,
                            "items": {"enum": ["linux", "macos", "windows"]}
                        }
                    }
                }
            }),
        )
        .unwrap()
    }

    fn accept(pairs: Vec<(&str, ElicitationValue)>) -> ElicitationResponse {
        ElicitationResponse::Accept {
            content: pairs
                .into_iter()
                .map(|(id, value)| (id.to_owned(), value))
                .collect(),
        }
    }

    #[test]
    fn answers_must_satisfy_the_requested_schema() {
        let request = numeric_request();
        assert!(
            request
                .validate_response(&accept(vec![("workers", ElicitationValue::Integer(4))]))
                .is_ok()
        );
        // A whole number for a `number` field decodes as an integer, so both
        // shapes have to reach the range check rather than the type error.
        assert!(
            request
                .validate_response(&accept(vec![
                    ("workers", ElicitationValue::Integer(4)),
                    ("ratio", ElicitationValue::Integer(1)),
                ]))
                .is_ok()
        );
        assert!(
            request
                .validate_response(&accept(vec![
                    ("workers", ElicitationValue::Integer(4)),
                    ("ratio", ElicitationValue::Number(1.5)),
                ]))
                .is_err()
        );
        assert!(
            request
                .validate_response(&accept(vec![("workers", ElicitationValue::Integer(9))]))
                .is_err()
        );
        assert!(
            request
                .validate_response(&accept(vec![(
                    "workers",
                    ElicitationValue::String("four".into())
                )]))
                .is_err()
        );
        // Required means required; declining and cancelling carry no content.
        assert!(request.validate_response(&accept(Vec::new())).is_err());
        assert!(
            request
                .validate_response(&ElicitationResponse::Decline)
                .is_ok()
        );
        assert!(
            request
                .validate_response(&ElicitationResponse::Cancel)
                .is_ok()
        );
    }

    #[test]
    fn multi_select_answers_honour_membership_and_item_counts() {
        let request = numeric_request();
        let with_targets = |values: Vec<&str>| {
            accept(vec![
                ("workers", ElicitationValue::Integer(2)),
                (
                    "targets",
                    ElicitationValue::StringArray(values.into_iter().map(str::to_owned).collect()),
                ),
            ])
        };
        assert!(
            request
                .validate_response(&with_targets(vec!["linux"]))
                .is_ok()
        );
        assert!(
            request
                .validate_response(&with_targets(vec!["linux", "macos", "windows"]))
                .is_err()
        );
        assert!(
            request
                .validate_response(&with_targets(Vec::new()))
                .is_err()
        );
        assert!(
            request
                .validate_response(&with_targets(vec!["plan9"]))
                .is_err()
        );
    }
}
