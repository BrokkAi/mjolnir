//! Resolves a recorded model value through the model list its profile's
//! harness advertised. A turn records the value it selected, and Claude's
//! values are aliases (`opus`, `fable`) whose meaning the harness decides; the
//! advertised list names the model each one runs ("Opus 5.5"). The newest
//! list is used for every period, so an alias the harness has since moved is
//! priced as its current model.
use super::pricing;
use mj_core::acp::SessionConfigChoice;
use std::collections::BTreeMap;

/// The newest discovered model list of each profile.
#[derive(Default)]
pub(super) struct ModelCatalogs(pub(super) BTreeMap<String, Vec<SessionConfigChoice>>);

impl ModelCatalogs {
    /// The model id to price `model` as: itself when it has a rate, else the
    /// priced model its profile's catalog says it runs, else itself.
    pub(super) fn priced_model(&self, profile: Option<&str>, model: String) -> String {
        if pricing::is_priced(&model) {
            return model;
        }
        profile
            .and_then(|profile| self.0.get(profile))
            .and_then(|choices| matching_choice(choices, &model))
            .and_then(choice_model_id)
            .filter(|id| pricing::is_priced(id))
            .unwrap_or(model)
    }
}

/// The choice `wanted` names: a listed value or display name, else, for a
/// family alias the list does not offer (Claude's `fable`), the family's
/// newest listed version, since an alias runs the newest model of its family.
fn matching_choice<'a>(
    choices: &'a [SessionConfigChoice],
    wanted: &str,
) -> Option<&'a SessionConfigChoice> {
    let wanted = wanted.trim();
    choices
        .iter()
        .find(|choice| {
            choice.value.eq_ignore_ascii_case(wanted) || choice.name.eq_ignore_ascii_case(wanted)
        })
        .or_else(|| {
            choices
                .iter()
                .filter_map(|choice| Some((family_version(choice)?, choice)))
                .filter(|((family, _), _)| family.eq_ignore_ascii_case(wanted))
                .max_by(|((_, a), _), ((_, b), _)| a.cmp(b))
                .map(|(_, choice)| choice)
        })
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

/// A full model id for a choice: its value when that is already one, else one
/// built from its display name ("Opus 5.5" is `claude-opus-5-5`).
fn choice_model_id(choice: &SessionConfigChoice) -> Option<String> {
    let value = match context_hint(&choice.value) {
        Some(hint) => &choice.value[..choice.value.len() - hint.len() - 2],
        None => &choice.value,
    };
    if pricing::is_priced(value) {
        return Some(value.to_owned());
    }
    let (family, version) = family_version(choice)?;
    let version = version.iter().map(u32::to_string).collect::<Vec<_>>();
    Some(format!(
        "claude-{}-{}",
        family.to_ascii_lowercase(),
        version.join("-")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(value: &str, name: &str) -> SessionConfigChoice {
        SessionConfigChoice {
            value: value.into(),
            name: name.into(),
            description: None,
        }
    }

    /// claude2's discovered list on the live store, which offers both the
    /// `opus` alias and the older `claude-opus-5`.
    fn claude_catalog() -> ModelCatalogs {
        ModelCatalogs(BTreeMap::from([(
            "claude".to_owned(),
            vec![
                choice("default", "Default (recommended)"),
                choice("opus", "Opus 5.5"),
                choice("claude-fable-5-1[1m]", "Fable 5.1"),
                choice("sonnet", "Sonnet 5"),
                choice("haiku", "Haiku 4.5"),
                choice("claude-opus-5", "Opus 5"),
                choice("claude-fable-5", "Fable 5"),
            ],
        )]))
    }

    #[test]
    fn claude_aliases_resolve_to_the_model_the_catalog_names() {
        let catalogs = claude_catalog();
        let priced = |model: &str| catalogs.priced_model(Some("claude"), model.into());
        assert_eq!(priced("opus"), "claude-opus-5-5");
        assert_eq!(priced("fable"), "claude-fable-5-1");
        assert_eq!(priced("sonnet"), "claude-sonnet-5");
        assert_eq!(priced("haiku"), "claude-haiku-4-5");
        assert_eq!(priced("claude-opus-5"), "claude-opus-5");
        // `fable` is not listed; it runs the family's newest listed version.
        // Unresolvable values and unknown profiles stay as recorded.
        assert_eq!(priced("default"), "default");
        assert_eq!(priced("custom-model"), "custom-model");
        assert_eq!(catalogs.priced_model(None, "opus".into()), "opus");
        assert_eq!(catalogs.priced_model(Some("other"), "opus".into()), "opus");
    }
}
