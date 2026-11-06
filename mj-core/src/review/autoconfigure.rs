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

/// One advertised model value and which enabled profiles offer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOffer {
    pub choice: SessionConfigChoice,
    /// Enabled profiles offering this exact value.
    pub profiles: usize,
    /// Those profiles that are not usage-priced. A profile whose billing is
    /// unknown counts as a subscription.
    pub subscription_profiles: usize,
}

/// Merge each enabled profile's advertised models, flagged usage-priced or
/// not, into one offer per distinct value, in first-seen order.
pub fn model_offers<'a>(
    profiles: impl IntoIterator<Item = (&'a [SessionConfigChoice], bool)>,
) -> Vec<ModelOffer> {
    let mut offers: Vec<ModelOffer> = Vec::new();
    for (models, usage_priced) in profiles {
        let mut seen = std::collections::BTreeSet::new();
        for choice in models {
            if !seen.insert(choice.value.as_str()) {
                continue;
            }
            let index = match offers
                .iter()
                .position(|offer| offer.choice.value == choice.value)
            {
                Some(index) => index,
                None => {
                    offers.push(ModelOffer {
                        choice: choice.clone(),
                        profiles: 0,
                        subscription_profiles: 0,
                    });
                    offers.len() - 1
                }
            };
            offers[index].profiles += 1;
            offers[index].subscription_profiles += usize::from(!usage_priced);
        }
    }
    offers
}

/// Offers from a merged model list whose sources are unknown: each model
/// counts as offered by one subscription profile. Enough to ask whether
/// autoconfigure has any answer; not to rank values by where they come from.
pub fn merged_model_offers(models: &[SessionConfigChoice]) -> Vec<ModelOffer> {
    model_offers([(models, false)])
}

/// Whether `profile_models` includes a model in a family that `drafted`
/// reviewers use. `all` is every profile's models merged, which names the
/// identities the families are read from.
pub fn offers_drafted_family(
    profile_models: &[SessionConfigChoice],
    all: &[SessionConfigChoice],
    drafted: &[SubagentChoice],
) -> bool {
    let family = |value: &str| {
        ReviewModelFamily::for_model(&crate::usage::model_catalog::model_id(all, value))
    };
    let drafted = drafted
        .iter()
        .filter_map(|choice| family(&choice.model))
        .collect::<Vec<_>>();
    profile_models
        .iter()
        .any(|model| family(&model.value).is_some_and(|family| drafted.contains(&family)))
}

/// `id` without a trailing `-YYYYMMDD` snapshot date.
fn undated(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => id,
    }
}

/// Choose review slots from the enabled profiles' model offers. Within a
/// family a value offered by a subscription profile beats one only
/// usage-priced profiles offer, then the value more enabled profiles offer
/// wins, then the newest version. Reviewers are load-balanced only across
/// profiles that offer their exact value, so this keeps reviews on
/// subscriptions and spread over as many profiles as possible.
pub fn autoconfigure(
    offers: &[ModelOffer],
    efforts: impl Fn(&str) -> Option<Vec<SessionConfigChoice>>,
) -> Option<Autoconfigured> {
    let models = offers
        .iter()
        .map(|offer| offer.choice.clone())
        .collect::<Vec<_>>();
    // Each family keeps its best (offer, identity) pair; identities decide
    // versions, values are what the reviewer is launched with.
    let mut selected: [Option<(&ModelOffer, String)>; 6] = Default::default();
    for offer in offers {
        let id = crate::usage::model_catalog::model_id(&models, &offer.choice.value);
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
        let better = current.as_ref().is_none_or(|(old_offer, old)| {
            // The plain Flash model is the general reviewer; max is a distinct
            // variant, not a better candidate, so billing and profile counts
            // never promote it.
            let plain = |identity: &str| !identity.contains("max");
            if index == 3 && plain(&id) != plain(old) {
                return plain(&id);
            }
            let subscribed = |offer: &ModelOffer| offer.subscription_profiles > 0;
            if subscribed(offer) != subscribed(old_offer) {
                return subscribed(offer);
            }
            if offer.profiles != old_offer.profiles {
                return offer.profiles > old_offer.profiles;
            }
            // A dated snapshot is the same version as its undated alias,
            // which follows the newest snapshot, so the alias wins a tie.
            let (new_base, old_base) = (undated(&id), undated(old));
            model_version_cmp(new_base, old_base)
                .then_with(|| (new_base.len() == id.len()).cmp(&(old_base.len() == old.len())))
                .is_gt()
        });
        if better {
            *current = Some((offer, id));
        }
    }
    let choice = |index: usize, effort: &str, adjusted: &mut Vec<EffortAdjustment>| {
        let model = selected[index].as_ref().unwrap().0.choice.value.clone();
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
            choice(2, "high", &mut adjusted),
            choice(2, "high", &mut adjusted),
            choice(2, "high", &mut adjusted),
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
        let known = autoconfigure(&merged_model_offers(&models), |_| Some(Vec::new())).unwrap();
        let unknown = autoconfigure(&merged_model_offers(&models), |_| None).unwrap();
        assert!(known.reviewers.iter().all(|choice| choice.effort.is_none()));
        assert!(
            unknown
                .reviewers
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("high"))
        );
    }

    #[test]
    fn known_efforts_choose_nearest_lower_then_lowest_and_report_adjustments() {
        let models = choices(&["gpt-6-terra", "gpt-6-sol"]);
        let configured = autoconfigure(&merged_model_offers(&models), |model| {
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
                &[
                    ("claude-haiku-4.5", "high"),
                    ("claude-haiku-4.5", "high"),
                    ("claude-haiku-4.5", "high"),
                ],
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
            let got = models(autoconfigure(&merged_model_offers(&choices(ids)), |_| None));
            let expected = expected
                .iter()
                .map(|(model, effort)| (model.to_string(), effort.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(got, expected, "{ids:?}");
        }
    }

    /// Autoconfigure over (usage_priced, models) profiles.
    fn pick(profiles: &[(bool, &[&str])]) -> Vec<(String, String)> {
        let lists = profiles
            .iter()
            .map(|(priced, ids)| (choices(ids), *priced))
            .collect::<Vec<_>>();
        models(autoconfigure(
            &model_offers(
                lists
                    .iter()
                    .map(|(models, priced)| (models.as_slice(), *priced)),
            ),
            |_| None,
        ))
    }
    fn expect(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(model, effort)| (model.to_string(), effort.to_string()))
            .collect()
    }

    /// Autoconfigure over named (usage_priced, [(value, name)]) profiles.
    fn pick_named(profiles: &[(bool, &[(&str, &str)])]) -> Vec<(String, String)> {
        let lists = profiles
            .iter()
            .map(|(priced, items)| {
                let models = items
                    .iter()
                    .map(|(value, name)| SessionConfigChoice {
                        value: (*value).into(),
                        name: (*name).into(),
                        description: None,
                    })
                    .collect::<Vec<_>>();
                (models, *priced)
            })
            .collect::<Vec<_>>();
        models(autoconfigure(
            &model_offers(
                lists
                    .iter()
                    .map(|(models, priced)| (models.as_slice(), *priced)),
            ),
            |_| None,
        ))
    }

    const CODEX: &[(&str, &str)] = &[
        ("gpt-6.1-terra", "gpt-6.1-terra"),
        ("gpt-6.1-sol", "gpt-6.1-sol"),
        ("gpt-6-sol", "gpt-6-sol"),
        ("gpt-5.6-terra", "gpt-5.6-terra"),
    ];
    const CLAUDE: &[(&str, &str)] = &[
        ("haiku", "Haiku 4.5"),
        ("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001"),
    ];
    const OPENROUTER: &[(&str, &str)] = &[
        ("openrouter/~openai/gpt-terra-latest", "GPT Terra (latest)"),
        ("openrouter/~openai/gpt-sol-latest", "GPT Sol (latest)"),
        (
            "openrouter/~anthropic/claude-haiku-latest",
            "Claude Haiku (latest)",
        ),
        ("openrouter/anthropic/claude-haiku-5.5", "Claude Haiku 5.5"),
        ("openrouter/openai/gpt-5.6-terra", "GPT 5.6 Terra"),
    ];

    #[test]
    fn subscription_models_beat_newer_latest_aliases_from_a_usage_priced_profile() {
        let expected = expect(&[
            ("gpt-6.1-terra", "xhigh"),
            ("gpt-6.1-sol", "medium"),
            ("haiku", "high"),
        ]);
        // The live catalog: four codex and six claude profiles, one OpenRouter profile.
        let mut profiles = vec![(true, OPENROUTER)];
        profiles.extend(std::iter::repeat_n((false, CODEX), 4));
        profiles.extend(std::iter::repeat_n((false, CLAUDE), 6));
        assert_eq!(pick_named(&profiles), expected);
        profiles.reverse();
        assert_eq!(pick_named(&profiles), expected);
        // One profile each ties the profile counts, so only the subscription
        // tier keeps the newer `latest` aliases and Haiku 5.5 from winning.
        assert_eq!(
            pick_named(&[(true, OPENROUTER), (false, CODEX), (false, CLAUDE)]),
            expected
        );
    }

    #[test]
    fn an_undated_alias_beats_its_own_dated_snapshot() {
        assert_eq!(
            pick_named(&[(
                false,
                &[
                    ("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001"),
                    ("haiku", "Haiku 4.5"),
                ],
            )])[0]
                .0,
            "haiku"
        );
    }

    #[test]
    fn usage_priced_models_are_used_when_no_subscription_offers_the_family() {
        assert_eq!(
            pick(&[
                (false, &["gpt-6-terra", "gpt-6-sol"]),
                (true, &["claude-haiku-4.5", "gpt-6.1-terra"]),
            ]),
            expect(&[
                ("gpt-6-terra", "xhigh"),
                ("gpt-6-sol", "medium"),
                ("claude-haiku-4.5", "high"),
            ])
        );
    }

    #[test]
    fn more_offering_profiles_beat_a_newer_version_within_subscriptions() {
        assert_eq!(
            pick(&[
                (false, &["gpt-6-terra", "gpt-6-sol", "claude-haiku-4.5"]),
                (false, &["gpt-6-terra", "gpt-6.1-sol", "claude-haiku-4.5"]),
                (false, &["gpt-6.1-terra", "gpt-6-sol", "claude-haiku-5"]),
            ]),
            expect(&[
                ("gpt-6-terra", "xhigh"),
                ("gpt-6-sol", "medium"),
                ("claude-haiku-4.5", "high"),
            ])
        );
    }

    #[test]
    fn newest_version_decides_between_equally_offered_subscription_models() {
        assert_eq!(
            pick(&[
                (false, &["gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"]),
                (false, &["gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"]),
            ]),
            expect(&[("gpt-6.1-terra", "xhigh"), ("gpt-6-sol", "medium")])
        );
    }

    #[test]
    fn usage_priced_profiles_do_not_raise_a_subscription_models_count() {
        // Three usage-priced profiles offer the other value; one subscription
        // offers this one. Subscription wins before counts are compared.
        assert_eq!(
            pick(&[
                (false, &["kimi-k3.1"]),
                (true, &["kimi-k3.2"]),
                (true, &["kimi-k3.2"]),
                (true, &["kimi-k3.2"]),
            ]),
            expect(&[("kimi-k3.1", "high")])
        );
    }

    #[test]
    fn plain_flash_beats_max_even_when_max_is_subscribed_or_more_offered() {
        let flash = ("deepseek-v3-flash", "high");
        for order in [false, true] {
            // Only a usage-priced profile offers plain Flash; a subscription
            // offers Max.
            let mut profiles: Vec<(bool, &[&str])> = vec![
                (true, &["deepseek-v3-flash"]),
                (false, &["deepseek-v4-flash-max"]),
            ];
            if order {
                profiles.reverse();
            }
            assert_eq!(pick(&profiles), expect(&[flash, flash]), "billing {order}");
            // Three profiles offer Max, one offers plain Flash.
            let mut profiles: Vec<(bool, &[&str])> = vec![
                (false, &["deepseek-v3-flash"]),
                (false, &["deepseek-v4-flash-max"]),
                (false, &["deepseek-v4-flash-max"]),
                (false, &["deepseek-v4-flash-max"]),
            ];
            if order {
                profiles.reverse();
            }
            assert_eq!(pick(&profiles), expect(&[flash, flash]), "count {order}");
        }
    }

    #[test]
    fn a_value_listed_twice_by_one_profile_counts_once() {
        let offers = model_offers([(choices(&["a", "a"]).as_slice(), false)]);
        assert_eq!(offers.len(), 1);
        assert_eq!(
            (offers[0].profiles, offers[0].subscription_profiles),
            (1, 1)
        );
    }
}
