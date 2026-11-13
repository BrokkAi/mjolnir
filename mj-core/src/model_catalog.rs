//! Shared interpretation of models advertised by harness profiles.
//! Identity names the model version; an offer retains the harness launch value.
use crate::acp::SessionConfigChoice;

fn is_claude_family(value: &str) -> bool {
    matches!(value, "opus" | "sonnet" | "haiku" | "fable" | "mythos")
}

fn context_hint(value: &str) -> Option<&str> {
    value
        .strip_suffix(']')?
        .rsplit_once('[')
        .map(|(_, hint)| hint)
}

/// The family and version of a "<Family> <version>" display name such as
/// "Opus 5.5" or "Claude Opus 5.5 (1M context)".
fn family_version(choice: &SessionConfigChoice) -> Option<(&str, Vec<u32>)> {
    let name = choice.name.trim();
    let name = name.strip_prefix("Claude ").unwrap_or(name);
    let name = match name.rsplit_once(" (") {
        Some((base, suffix)) if suffix.ends_with(" context)") => base,
        _ => name,
    };
    let (family, version) = name.split_once(' ')?;
    if family.is_empty() || !family.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let version = version
        .split('.')
        .map(|part| {
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect::<Option<Vec<u32>>>()?;
    Some((family, version))
}

/// The model identity a harness says an advertised choice runs. Context
/// hints remain part of selection identity; pricing can discard them later.
pub fn choice_model_id(choice: &SessionConfigChoice) -> String {
    let value = normalize_model(&choice.value);
    let base = value.split('[').next().unwrap_or(&value);
    if is_claude_family(base)
        && let Some((family, version)) = family_version(choice)
        && family.eq_ignore_ascii_case(base)
    {
        let version = version.iter().map(u32::to_string).collect::<Vec<_>>();
        let hint = context_hint(&value)
            .map(|hint| format!("[{hint}]"))
            .unwrap_or_default();
        return format!(
            "claude-{}-{}{hint}",
            family.to_ascii_lowercase(),
            version.join("-")
        );
    }
    value
}

/// Resolve a request once against all available advertised catalogs. If
/// profiles disagree about a moving alias, its newest advertised version wins.
/// Unknown values remain opaque; explicit versions never follow another version.
pub fn model_id(choices: &[SessionConfigChoice], wanted: &str) -> String {
    let wanted = wanted.trim();
    choices
        .iter()
        .filter(|choice| {
            choice.value.eq_ignore_ascii_case(wanted) || choice.name.eq_ignore_ascii_case(wanted)
        })
        .map(choice_model_id)
        .max_by(|a, b| model_version_cmp(a, b))
        .or_else(|| {
            is_claude_family(&wanted.to_ascii_lowercase())
                .then(|| {
                    choices
                        .iter()
                        .filter_map(|choice| Some((family_version(choice)?, choice)))
                        .filter(|((family, _), _)| family.eq_ignore_ascii_case(wanted))
                        .max_by(|((_, a), left), ((_, b), right)| {
                            a.cmp(b)
                                .then_with(|| {
                                    context_hint(&right.value)
                                        .is_some()
                                        .cmp(&context_hint(&left.value).is_some())
                                })
                                .then_with(|| left.value.cmp(&right.value))
                        })
                        .map(|(_, choice)| choice_model_id(choice))
                })
                .flatten()
        })
        .unwrap_or_else(|| normalize_model(wanted))
}

/// One identity and the exact advertised value to use with a profile's harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOffer {
    pub identity: String,
    pub value: String,
}

/// Find a profile's launch value for an already resolved identity. Prefer the
/// requested spelling when offered, then the identity itself, then a stable value.
pub fn offer(
    choices: &[SessionConfigChoice],
    identity: &str,
    preferred: &str,
) -> Option<ModelOffer> {
    choices
        .iter()
        .filter(|choice| choice_model_id(choice) == identity)
        .min_by_key(|choice| {
            (
                choice.value != preferred,
                choice.value != identity,
                &choice.value,
            )
        })
        .map(|choice| ModelOffer {
            identity: identity.to_owned(),
            value: choice.value.clone(),
        })
}

/// Resolve a request and find its advertised launch value in one catalog.
pub fn resolve(choices: &[SessionConfigChoice], wanted: &str) -> Option<ModelOffer> {
    offer(choices, &model_id(choices, wanted), wanted)
}

/// Normalize provider qualifiers without losing versions or context requirements.
pub fn normalize_model(model: &str) -> String {
    let lowercase = model.to_ascii_lowercase();
    let model = lowercase.as_str();
    // Known provider qualifiers name the same underlying model.
    let model = model
        .strip_prefix("global.")
        .or_else(|| model.strip_prefix("us."))
        .or_else(|| model.strip_prefix("eu."))
        .unwrap_or(model);
    let model = model
        .strip_prefix("openai.")
        .or_else(|| model.strip_prefix("anthropic."))
        .unwrap_or(model);
    let model = model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("anthropic/"))
        .or_else(|| model.strip_prefix("z-ai/"))
        .or_else(|| model.strip_prefix("deepseek/"))
        .or_else(|| model.strip_prefix("x-ai/"))
        .or_else(|| model.strip_prefix("moonshotai/"))
        .unwrap_or(model);
    let model = model.strip_suffix(":0").unwrap_or(model);
    model.to_owned()
}

/// Shared with utility inference: moving aliases precede numbered versions.
pub fn model_version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let alias = |id: &str| {
        u8::from(
            id.split(['-', '_', '.'])
                .any(|p| matches!(p, "latest" | "next")),
        )
    };
    let numbers = |id: &str| {
        // A context capacity such as K3's `-256k` is not a release version.
        id.split(['-', '_', '.', '/'])
            .filter(|part| {
                !part
                    .strip_suffix('k')
                    .or_else(|| part.strip_suffix('m'))
                    .is_some_and(|size| {
                        !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit())
                    })
            })
            .flat_map(|part| part.split(|c: char| !c.is_ascii_digit()))
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse::<u64>().ok())
            .collect::<Vec<_>>()
    };
    alias(left)
        .cmp(&alias(right))
        .then_with(|| numbers(left).cmp(&numbers(right)))
        // Highspeed is a same-version speed variant, not a newer model.
        .then_with(|| {
            right
                .ends_with("-highspeed")
                .cmp(&left.ends_with("-highspeed"))
        })
        .then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1534: independently interpreting a moving alias can mix model versions.
    #[test]
    fn moving_alias_resolves_once_and_preserves_versions_context_and_unknown_ids() {
        let recorded: BTreeMap<String, Vec<SessionConfigChoice>> = serde_json::from_str(
            include_str!("../tests/fixtures/claude-antigravity-models.json"),
        )
        .unwrap();
        let mut models = recorded.values().flatten().cloned().collect::<Vec<_>>();
        let mut stale = recorded["claude"][1].clone();
        stale.name = "Sonnet 5".into();
        models.push(stale.clone());
        for reverse in [false, true] {
            if reverse {
                models.reverse();
            }
            assert_eq!(model_id(&models, "sonnet"), "claude-sonnet-5-5");
            assert!(offer(std::slice::from_ref(&stale), "claude-sonnet-5-5", "sonnet").is_none());
            assert_eq!(
                resolve(&recorded["claude"], "claude-sonnet-5-5")
                    .unwrap()
                    .value,
                "sonnet"
            );
            assert_eq!(
                resolve(&recorded["antigravity"], "sonnet").unwrap().value,
                "claude-sonnet-5-5"
            );
            assert_eq!(model_id(&models, "claude-sonnet-5"), "claude-sonnet-5");
        }
        let context = SessionConfigChoice {
            value: "sonnet[1m]".into(),
            name: "Sonnet 5.5 (1M context)".into(),
            description: None,
        };
        assert_eq!(choice_model_id(&context), "claude-sonnet-5-5[1m]");
        assert!(offer(&[context], "claude-sonnet-5-5", "sonnet").is_none());
        assert!(resolve(&models, "claude-sonnet-5-5[1m]").is_none());
        let opaque = SessionConfigChoice {
            value: "private-router-model".into(),
            name: "Sonnet 5.5".into(),
            description: None,
        };
        assert_eq!(choice_model_id(&opaque), "private-router-model");
        assert!(offer(&[opaque], "claude-sonnet-5-5", "sonnet").is_none());
        assert_eq!(model_id(&models, "unknown-model"), "unknown-model");
    }

    use std::collections::BTreeMap;
}
