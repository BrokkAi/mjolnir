//! Ordered reviewer defaults from advertised enabled-profile models.
use crate::acp::SessionConfigChoice;
use crate::config::SubagentChoice;
use crate::review::settings::{ReviewModelFamily, model_version_cmp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortAdjustment {
    pub model: String,
    pub requested: String,
    pub used: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Autoconfigured {
    pub reviewers: Vec<SubagentChoice>,
    pub adjusted: Vec<EffortAdjustment>,
}

/// Use the closest offered effort below the requested one, or the lowest
/// offered effort if every choice is higher.
pub fn fallback_effort(requested: &str, choices: &[SessionConfigChoice]) -> Option<String> {
    let rank = |value: &str| {
        ["minimal", "low", "medium", "high", "xhigh", "max"]
            .iter()
            .position(|item| *item == value)
    };
    if choices.iter().any(|choice| choice.value == requested) {
        return Some(requested.to_owned());
    }
    let requested_rank = rank(requested)?;
    let ranked = choices
        .iter()
        .filter_map(|choice| rank(&choice.value).map(|rank| (rank, &choice.value)));
    ranked
        .clone()
        .filter(|(rank, _)| *rank < requested_rank)
        .max_by_key(|(rank, _)| *rank)
        .or_else(|| ranked.min_by_key(|(rank, _)| *rank))
        .map(|(_, value)| value.clone())
}

/// Choose review slots from the union of enabled profiles' model choices.
pub fn autoconfigure(
    models: &[SessionConfigChoice],
    efforts: impl Fn(&str) -> Option<Vec<SessionConfigChoice>>,
) -> Option<Autoconfigured> {
    // Each family keeps its best (identity, advertised value) pair; identities
    // decide, values are what the reviewer is launched with.
    let mut selected: [Option<(String, String)>; 6] = Default::default();
    for choice in models {
        let id = crate::usage::model_catalog::model_id(models, &choice.value);
        let Some(family) = ReviewModelFamily::for_model(&id) else {
            continue;
        };
        let index = match family {
            ReviewModelFamily::Terra => 0,
            ReviewModelFamily::Sol => 1,
            ReviewModelFamily::Haiku => 2,
            ReviewModelFamily::Flash => 3,
            ReviewModelFamily::Grok => 4,
            ReviewModelFamily::Kimi => 5,
        };
        let current = &mut selected[index];
        // The plain Flash model is the general reviewer; max is a distinct variant.
        let better = current.as_ref().is_none_or(|(old, _)| {
            let plain = |identity: &str| !identity.contains("max");
            if index == 3 && plain(&id) != plain(old) {
                plain(&id)
            } else {
                model_version_cmp(&id, old).is_gt()
            }
        });
        if better {
            *current = Some((id, choice.value.clone()));
        }
    }
    let choice = |index: usize, effort: &str, adjusted: &mut Vec<EffortAdjustment>| {
        let model = selected[index].as_ref().unwrap().1.clone();
        let offered = efforts(&model);
        let used = offered
            .map(|choices| fallback_effort(effort, &choices))
            .unwrap_or_else(|| Some(effort.to_owned()));
        if used.as_deref() != Some(effort) {
            adjusted.push(EffortAdjustment {
                model: model.clone(),
                requested: effort.to_owned(),
                used: used.clone(),
            });
        }
        SubagentChoice {
            model,
            effort: used,
        }
    };
    let mut adjusted = Vec::new();
    let reviewers = if selected[0].is_some() && selected[1].is_some() && selected[2].is_some() {
        vec![
            choice(0, "xhigh", &mut adjusted),
            choice(1, "medium", &mut adjusted),
            choice(2, "high", &mut adjusted),
        ]
    } else if selected[0].is_some() && selected[1].is_some() {
        vec![
            choice(0, "xhigh", &mut adjusted),
            choice(1, "medium", &mut adjusted),
        ]
    } else if selected[2].is_some() {
        vec![
            choice(2, "xhigh", &mut adjusted),
            choice(2, "xhigh", &mut adjusted),
        ]
    } else if selected[3].is_some() {
        vec![
            choice(3, "high", &mut adjusted),
            choice(3, "high", &mut adjusted),
        ]
    } else if selected[4].is_some() {
        vec![choice(4, "high", &mut adjusted)]
    } else if selected[5].is_some() {
        vec![choice(5, "high", &mut adjusted)]
    } else {
        return None;
    };
    Some(Autoconfigured {
        reviewers,
        adjusted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(ids: &[&str]) -> Vec<SessionConfigChoice> {
        ids.iter()
            .map(|id| SessionConfigChoice {
                value: (*id).into(),
                name: (*id).into(),
                description: None,
            })
            .collect()
    }
    fn models(result: Option<Autoconfigured>) -> Vec<(String, String)> {
        result
            .unwrap_or_default()
            .reviewers
            .into_iter()
            .map(|choice| (choice.model, choice.effort.unwrap()))
            .collect()
    }

    #[test]
    fn known_empty_haiku_efforts_use_harness_default_but_unknown_keeps_table_effort() {
        let models = choices(&["claude-haiku-4.5"]);
        let known = autoconfigure(&models, |_| Some(Vec::new())).unwrap();
        let unknown = autoconfigure(&models, |_| None).unwrap();
        assert!(known.reviewers.iter().all(|choice| choice.effort.is_none()));
        assert!(
            unknown
                .reviewers
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("xhigh"))
        );
    }

    #[test]
    fn known_efforts_choose_nearest_lower_then_lowest_and_report_adjustments() {
        let models = choices(&["gpt-6-terra", "gpt-6-sol"]);
        let configured = autoconfigure(&models, |model| {
            Some(if model.ends_with("terra") {
                choices(&["low", "high", "max"])
            } else {
                choices(&["high", "xhigh"])
            })
        })
        .unwrap();
        assert_eq!(configured.reviewers[0].effort.as_deref(), Some("high"));
        assert_eq!(configured.reviewers[1].effort.as_deref(), Some("high"));
        assert_eq!(configured.adjusted.len(), 2);
        assert_eq!(configured.adjusted[0].requested, "xhigh");
        assert_eq!(configured.adjusted[0].used.as_deref(), Some("high"));
        assert_eq!(configured.adjusted[1].requested, "medium");
        assert_eq!(configured.adjusted[1].used.as_deref(), Some("high"));
    }

    #[test]
    fn autoconfigure_uses_first_matching_row_and_newest_family_versions() {
        type Case = (
            &'static [&'static str],
            &'static [(&'static str, &'static str)],
        );
        let cases: &[Case] = &[
            (
                &[
                    "gpt-6-terra",
                    "gpt-6.10-terra",
                    "gpt-6.1-sol",
                    "claude-haiku-4.5",
                    "grok-4.6",
                ],
                &[
                    ("gpt-6.10-terra", "xhigh"),
                    ("gpt-6.1-sol", "medium"),
                    ("claude-haiku-4.5", "high"),
                ],
            ),
            (
                &["gpt-6-terra", "gpt-6-sol", "grok-4.6"],
                &[("gpt-6-terra", "xhigh"), ("gpt-6-sol", "medium")],
            ),
            (
                &["claude-haiku-4.5", "deepseek-v3-flash"],
                &[("claude-haiku-4.5", "xhigh"), ("claude-haiku-4.5", "xhigh")],
            ),
            (
                &["deepseek-v3-flash", "deepseek-v4-flash-max"],
                &[("deepseek-v3-flash", "high"), ("deepseek-v3-flash", "high")],
            ),
            (
                &["grok-4.6", "kimi-k3.1", "grok-4.20-multi-agent-0309"],
                &[("grok-4.6", "high")],
            ),
            (&["kimi-k3.1", "kimi-k3.2"], &[("kimi-k3.2", "high")]),
            (&["unrelated-model", "kimi-k3"], &[]),
        ];
        for (ids, expected) in cases {
            let got = models(autoconfigure(&choices(ids), |_| None));
            let expected = expected
                .iter()
                .map(|(model, effort)| (model.to_string(), effort.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(got, expected, "{ids:?}");
        }
    }
}
