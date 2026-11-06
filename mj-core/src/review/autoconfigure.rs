//! Ordered reviewer defaults from advertised enabled-profile models.
use crate::acp::SessionConfigChoice;
use crate::config::SubagentChoice;
use crate::review::settings::{ReviewModelFamily, model_version_cmp};

/// Choose review slots from the union of enabled profiles' model choices.
pub fn autoconfigure(models: &[SessionConfigChoice]) -> Option<Vec<SubagentChoice>> {
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
    let choice = |index: usize, effort: &str| SubagentChoice {
        model: selected[index].as_ref().unwrap().1.clone(),
        effort: Some(effort.into()),
    };
    if selected[0].is_some() && selected[1].is_some() && selected[2].is_some() {
        Some(vec![
            choice(0, "xhigh"),
            choice(1, "medium"),
            choice(2, "high"),
        ])
    } else if selected[0].is_some() && selected[1].is_some() {
        Some(vec![choice(0, "xhigh"), choice(1, "medium")])
    } else if selected[2].is_some() {
        Some(vec![choice(2, "xhigh"), choice(2, "xhigh")])
    } else if selected[3].is_some() {
        Some(vec![choice(3, "high"), choice(3, "high")])
    } else if selected[4].is_some() {
        Some(vec![choice(4, "high")])
    } else if selected[5].is_some() {
        Some(vec![choice(5, "high")])
    } else {
        None
    }
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
    fn models(result: Option<Vec<SubagentChoice>>) -> Vec<(String, String)> {
        result
            .unwrap_or_default()
            .into_iter()
            .map(|choice| (choice.model, choice.effort.unwrap()))
            .collect()
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
            let got = models(autoconfigure(&choices(ids)));
            let expected = expected
                .iter()
                .map(|(model, effort)| (model.to_string(), effort.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(got, expected, "{ids:?}");
        }
    }
}
