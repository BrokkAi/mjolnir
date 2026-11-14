//! Ordered reviewer defaults from advertised enabled-profile models.
use crate::acp::SessionConfigChoice;
use crate::config::SubagentChoice;
use crate::model_catalog::{self, model_version_cmp};
use crate::review::settings::ReviewModelFamily;
use std::collections::BTreeMap;

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
    pub identity: String,
    /// Enabled profiles offering this model identity.
    pub profiles: usize,
    /// Those profiles that are not usage-priced. A profile whose billing is
    /// unknown counts as a subscription.
    pub subscription_profiles: usize,
}

/// Merge each enabled profile's advertised models, flagged usage-priced or
/// not, into one offer per distinct identity, in first-seen order.
pub fn model_offers<'a>(
    profiles: impl IntoIterator<Item = (&'a [SessionConfigChoice], bool)>,
) -> Vec<ModelOffer> {
    let mut offers: Vec<ModelOffer> = Vec::new();
    for (models, usage_priced) in profiles {
        let mut seen = std::collections::BTreeSet::new();
        for choice in models {
            let identity = model_catalog::choice_model_id(choice);
            if !seen.insert(identity.clone()) {
                continue;
            }
            let index = match offers.iter().position(|offer| offer.identity == identity) {
                Some(index) => index,
                None => {
                    offers.push(ModelOffer {
                        choice: choice.clone(),
                        identity,
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
    let choices = model_catalog::merged_choices(offers.iter().map(|offer| offer.choice.clone()));
    for (offer, choice) in offers.iter_mut().zip(choices) {
        offer.choice = choice;
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
    let family = |value: &str| ReviewModelFamily::for_model(&model_catalog::model_id(all, value));
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

/// Full models precede reduced variants even when the latter are newer or
/// offered by more profiles. Opaque advertised models remain candidates.
fn full_model(id: &str) -> bool {
    !id.split(['-', '_', '.', '/']).any(|part| {
        matches!(
            part,
            "flash"
                | "flashx"
                | "mini"
                | "nano"
                | "lite"
                | "small"
                | "haiku"
                | "luna"
                | "dsf"
                | "dsfmax"
                | "default"
                | "auto"
        )
    })
}

/// Choose review slots from the enabled profiles' model offers. Within a
/// family full models precede reduced variants, then a subscription value
/// beats one only usage-priced profiles offer, then the value offered by
/// more enabled profiles wins, then the newest version. Reviewers are load-balanced only across
/// profiles that offer the same resolved identity, so this keeps reviews on
/// subscriptions and spread over as many profiles as possible.
pub fn autoconfigure(
    offers: &[ModelOffer],
    efforts: impl Fn(&str) -> Option<Vec<SessionConfigChoice>>,
) -> Option<Autoconfigured> {
    // Each family keeps its best (offer, identity) pair; identities decide
    // versions, values are what the reviewer is launched with.
    let mut selected: BTreeMap<ReviewModelFamily, (&ModelOffer, String)> = BTreeMap::new();
    for offer in offers {
        let id = offer.identity.clone();
        let Some(family) = ReviewModelFamily::for_model(&id) else {
            continue;
        };
        let better = selected.get(&family).is_none_or(|(old_offer, old)| {
            if full_model(&id) != full_model(old) {
                return full_model(&id);
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
            selected.insert(family, (offer, id));
        }
    }
    let choice =
        |family: &ReviewModelFamily, effort: &str, adjusted: &mut Vec<EffortAdjustment>| {
            let model = selected[family].0.choice.value.clone();
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
    use ReviewModelFamily::{DeepSeek, Grok, Kimi, Sol, Sonnet, Terra};
    let preset = if selected.contains_key(&Terra) && selected.contains_key(&Sol) {
        vec![(Terra, "high"); 3]
    } else {
        let family = [Sonnet, DeepSeek, Grok, Kimi]
            .into_iter()
            .find(|family| selected.contains_key(family))
            .or_else(|| {
                selected
                    .iter()
                    .max_by(
                        |(family, (offer, id)), (other_family, (other_offer, other_id))| {
                            full_model(id)
                                .cmp(&full_model(other_id))
                                .then_with(|| {
                                    (offer.subscription_profiles > 0)
                                        .cmp(&(other_offer.subscription_profiles > 0))
                                })
                                .then_with(|| offer.profiles.cmp(&other_offer.profiles))
                                // Provider versions are incomparable; names only
                                // break ties between otherwise equal providers.
                                .then_with(|| other_family.cmp(family))
                        },
                    )
                    .map(|(family, _)| family.clone())
            })?;
        vec![(family, "high"); 2]
    };
    let mut adjusted = Vec::new();
    let reviewers = preset
        .into_iter()
        .map(|(family, effort)| choice(&family, effort, &mut adjusted))
        .collect();
    Some(Autoconfigured {
        reviewers,
        adjusted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1534: alias spellings hid shared capacity and could double-count a profile.
    #[test]
    fn model_offers_count_each_profile_once_per_identity_across_harness_aliases() {
        let recorded: BTreeMap<String, Vec<SessionConfigChoice>> = serde_json::from_str(
            include_str!("../../tests/fixtures/claude-antigravity-models.json"),
        )
        .unwrap();
        let mut claude = recorded["claude"].clone();
        claude.push(recorded["antigravity"][2].clone());
        let offers = model_offers([(&claude[..], false), (&recorded["antigravity"][..], false)]);
        let sonnet = offers
            .iter()
            .find(|offer| offer.identity == "claude-sonnet-5-5")
            .unwrap();
        assert_eq!((sonnet.profiles, sonnet.subscription_profiles), (2, 2));
        assert_eq!(
            offers
                .iter()
                .find(|offer| offer.identity == "claude-sonnet-5")
                .unwrap()
                .profiles,
            1
        );
        // An older, more widely offered alias must remain pinned to that
        // version when Autoconfigure prefers its larger subscription pool.
        let mut stale = recorded["claude"].clone();
        stale[1].name = "Sonnet 5".into();
        let offers = model_offers([
            (&stale[..], false),
            (&stale[..], false),
            (&claude[..], false),
        ]);
        assert_eq!(
            autoconfigure(&offers, |_| None).unwrap().reviewers[0].model,
            "claude-sonnet-5"
        );
    }

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
    fn known_empty_sonnet_efforts_use_harness_default_but_unknown_keeps_table_effort() {
        let models = choices(&["claude-sonnet-4.5"]);
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
        let nearest_lower = autoconfigure(
            &merged_model_offers(&choices(&["gpt-6-terra", "gpt-6-sol"])),
            |_| Some(choices(&["low", "max"])),
        )
        .unwrap();
        assert!(
            nearest_lower
                .reviewers
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("low"))
        );
        assert_eq!(nearest_lower.adjusted.len(), 3);
        assert_eq!(nearest_lower.adjusted[0].requested, "high");
        assert_eq!(nearest_lower.adjusted[0].used.as_deref(), Some("low"));
        let lowest = autoconfigure(
            &merged_model_offers(&choices(&["claude-sonnet-4.5"])),
            |_| Some(choices(&["xhigh", "max"])),
        )
        .unwrap();
        assert!(
            lowest
                .reviewers
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("xhigh"))
        );
        assert_eq!(lowest.adjusted.len(), 2);
        assert_eq!(lowest.adjusted[1].used.as_deref(), Some("xhigh"));
    }

    // Hard-won: ed508ae8c: stripping Kimi Code's provider selected an older Kimi model.
    // Hard-won: 9541bf079: K3's 256k context suffix outranked the newer K3.1 Pro release.
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
                    "claude-sonnet-4.5",
                    "grok-4.6",
                ],
                &[
                    ("gpt-6.10-terra", "high"),
                    ("gpt-6.10-terra", "high"),
                    ("gpt-6.10-terra", "high"),
                ],
            ),
            (
                &["gpt-6-terra", "gpt-6-sol", "grok-4.6"],
                &[
                    ("gpt-6-terra", "high"),
                    ("gpt-6-terra", "high"),
                    ("gpt-6-terra", "high"),
                ],
            ),
            (
                &["claude-sonnet-4.5", "deepseek-v4-pro"],
                &[("claude-sonnet-4.5", "high"), ("claude-sonnet-4.5", "high")],
            ),
            (
                &[
                    "deepseek-v4-pro",
                    "deepseek-v4-flash",
                    "deepseek-v4-flash-max",
                ],
                &[("deepseek-v4-pro", "high"), ("deepseek-v4-pro", "high")],
            ),
            (
                &[
                    "grok-4.6",
                    "kimi-code/k3.1-pro",
                    "grok-4.20-multi-agent-0309",
                ],
                &[("grok-4.6", "high"), ("grok-4.6", "high")],
            ),
            (
                // The live Kimi Code catalog, including an older model with
                // a larger context and the same-version Highspeed variant.
                &[
                    "kimi-code/kimi-for-coding",
                    "kimi-code/kimi-for-coding-highspeed",
                    "kimi-code/k3",
                    "kimi-code/k3.1-pro",
                    "kimi-code/k3-256k",
                    "kimi-code/k3.1-pro-highspeed",
                ],
                &[
                    ("kimi-code/k3.1-pro", "high"),
                    ("kimi-code/k3.1-pro", "high"),
                ],
            ),
            (
                &["unrelated-model", "kimi-k3"],
                &[("kimi-k3", "high"), ("kimi-k3", "high")],
            ),
            (
                &["kimi-k2.7-code", "kimi-code/k3"],
                &[("kimi-code/k3", "high"), ("kimi-code/k3", "high")],
            ),
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
        ("sonnet", "Sonnet 4.5"),
        ("claude-sonnet-4-5-20251001", "claude-sonnet-4-5-20251001"),
    ];
    const OPENROUTER: &[(&str, &str)] = &[
        ("openrouter/~openai/gpt-terra-latest", "GPT Terra (latest)"),
        ("openrouter/~openai/gpt-sol-latest", "GPT Sol (latest)"),
        (
            "openrouter/~anthropic/claude-sonnet-latest",
            "Claude Sonnet (latest)",
        ),
        (
            "openrouter/anthropic/claude-sonnet-5.5",
            "Claude Sonnet 5.5",
        ),
        ("openrouter/openai/gpt-5.6-terra", "GPT 5.6 Terra"),
    ];

    #[test]
    fn subscription_models_beat_newer_latest_aliases_from_a_usage_priced_profile() {
        let expected = expect(&[("gpt-6.1-terra", "high"); 3]);
        // The live catalog: four codex and six claude profiles, one OpenRouter profile.
        let mut profiles = vec![(true, OPENROUTER)];
        profiles.extend(std::iter::repeat_n((false, CODEX), 4));
        profiles.extend(std::iter::repeat_n((false, CLAUDE), 6));
        assert_eq!(pick_named(&profiles), expected);
        profiles.reverse();
        assert_eq!(pick_named(&profiles), expected);
        // One profile each ties the profile counts, so only the subscription
        // tier keeps the newer `latest` aliases and Sonnet 5.5 from winning.
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
                    ("claude-sonnet-4-5-20251001", "claude-sonnet-4-5-20251001"),
                    ("sonnet", "Sonnet 4.5"),
                ],
            )])[0]
                .0,
            "sonnet"
        );
    }

    #[test]
    fn usage_priced_models_are_used_when_no_subscription_offers_the_family() {
        assert_eq!(
            pick(&[
                (false, &["grok-4.6"]),
                (true, &["claude-sonnet-4.5", "gpt-6.1-terra"]),
            ]),
            expect(&[("claude-sonnet-4.5", "high"), ("claude-sonnet-4.5", "high")])
        );
    }

    #[test]
    fn more_offering_profiles_beat_a_newer_version_within_subscriptions() {
        assert_eq!(
            pick(&[
                (false, &["gpt-6-terra", "gpt-6-sol", "claude-sonnet-4.5"]),
                (false, &["gpt-6-terra", "gpt-6.1-sol", "claude-sonnet-4.5"]),
                (false, &["gpt-6.1-terra", "gpt-6-sol", "claude-sonnet-5"]),
            ]),
            expect(&[("gpt-6-terra", "high"); 3])
        );
    }

    #[test]
    fn newest_version_decides_between_equally_offered_subscription_models() {
        assert_eq!(
            pick(&[
                (false, &["gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"]),
                (false, &["gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"]),
            ]),
            expect(&[("gpt-6.1-terra", "high"); 3])
        );
    }

    #[test]
    fn usage_priced_profiles_do_not_raise_a_subscription_models_count() {
        // Three usage-priced profiles offer the other value; one subscription
        // offers this one. Subscription wins before counts are compared.
        assert_eq!(
            pick(&[
                (false, &["kimi-code/k3"]),
                (true, &["kimi-code/k3.1-pro"]),
                (true, &["kimi-code/k3.1-pro"]),
                (true, &["kimi-code/k3.1-pro"]),
            ]),
            expect(&[("kimi-code/k3", "high"), ("kimi-code/k3", "high")])
        );
    }

    #[test]
    fn flagship_pro_beats_flash_even_when_flash_is_subscribed_or_more_offered() {
        let pro = ("deepseek-v4-pro", "high");
        for order in [false, true] {
            // Only a usage-priced profile offers Pro; a subscription
            // offers Flash Max.
            let mut profiles: Vec<(bool, &[&str])> = vec![
                (true, &["deepseek-v4-pro"]),
                (false, &["deepseek-v4-flash-max"]),
            ];
            if order {
                profiles.reverse();
            }
            assert_eq!(pick(&profiles), expect(&[pro, pro]), "billing {order}");
            // Three profiles offer Flash Max, one offers Pro.
            let mut profiles: Vec<(bool, &[&str])> = vec![
                (false, &["deepseek-v4-pro"]),
                (false, &["deepseek-v4-flash-max"]),
                (false, &["deepseek-v4-flash-max"]),
                (false, &["deepseek-v4-flash-max"]),
            ];
            if order {
                profiles.reverse();
            }
            assert_eq!(pick(&profiles), expect(&[pro, pro]), "count {order}");
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
