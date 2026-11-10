//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::controller::Controller;
use crate::server::api::{
    SubagentBackend, resolve_pinned_reviewer_model_profile_selection,
    resolve_reviewer_model_profile_selection,
};
use crate::session_manager::ManagedSessionHandle;
use anyhow::{Context, Result, ensure};
use futures::future::join_all;
use mj_core::config::{
    LegacyReview, ReviewConfig, SessionReview, SubagentChoice, apply_legacy_override,
};
use mj_core::review::autoconfigure::{
    Autoconfigured, autoconfigure, merged_model_offers, model_offers, offers_drafted_family,
};
use mj_core::review::settings::{ResolvedReviewSettings, ReviewModelSettings};
use mj_core::worker_launch::ProfileConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    cancelled: Arc<AtomicBool>,
    backend: Arc<dyn SubagentBackend>,
) -> Result<Vec<ResolvedReviewSettings>> {
    let controller = tokio::task::spawn_blocking(Controller::load).await??;
    let settings = settings.unwrap_or_else(|| controller.config.review.clone());
    let session = controller
        .state
        .sessions
        .get(handle.session_id())
        .context("review session is missing")?;
    ensure!(!session.archived, "this session is archived");
    let choice = controller
        .state
        .effective_review(handle.session_id())
        .map_err(|session| {
            anyhow::anyhow!(
                "the review settings of session {session} cannot be resolved: its parent is missing or the parent chain repeats"
            )
        })?;
    let (mut choices, autoconfigured) =
        configured_choices(settings.clone(), &controller.config, &cancelled, &backend).await?;
    if autoconfigured
        && let Some(SessionReview::Legacy {
            count,
            model,
            effort,
            ..
        }) = choice
    {
        choices =
            apply_legacy_override(&choices, *count, model.as_ref(), effort.as_ref(), &settings);
    }
    let pinned_profile = if autoconfigured {
        settings
            .legacy
            .as_ref()
            .and_then(|legacy| legacy.profile.as_ref())
            .filter(|profile| controller.config.enabled_profile(profile).is_some())
    } else {
        None
    };
    let selections = join_all(choices.into_iter().map(|SubagentChoice { model, effort }| {
        let backend = Arc::clone(&backend);
        let cancelled = Arc::clone(&cancelled);
        let pinned_profile = pinned_profile.cloned();
        async move {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "review preparation cancelled"
            );
            let selected = await_or_cancel(&cancelled, async {
                if let Some(profile) = pinned_profile.as_ref() {
                    resolve_pinned_reviewer_model_profile_selection(
                        &backend,
                        profile,
                        &model,
                        effort.as_deref(),
                    )
                    .await
                } else {
                    resolve_reviewer_model_profile_selection(&backend, &model, effort.as_deref())
                        .await
                }
            })
            .await?
            .map_err(|error| anyhow::anyhow!("reviewer model {model:?}: {}", error.message))?;
            Ok(ResolvedReviewSettings {
                profile: selected.profile_id,
                main: ReviewModelSettings {
                    model: Some(selected.model),
                    effort: selected.effort,
                    fast_mode: false,
                },
                autoconfigured,
            })
        }
    }))
    .await;
    selections.into_iter().collect()
}

async fn configured_choices(
    settings: ReviewConfig,
    config: &mj_core::config::Config,
    cancelled: &AtomicBool,
    backend: &Arc<dyn SubagentBackend>,
) -> Result<(Vec<SubagentChoice>, bool)> {
    let autoconfigured = settings.is_legacy();
    let choices = if autoconfigured {
        let pinned = settings
            .legacy
            .as_ref()
            .and_then(|legacy| legacy.profile.as_ref())
            .filter(|profile| config.enabled_profile(profile).is_some());
        let ready =
            await_or_cancel(cancelled, backend.reviewer_ready_candidates(String::new())).await??;
        let mut usage_priced = usage_priced_profiles(&ready);
        let candidates = if let Some(profile) = pinned {
            if let Some(candidate) = ready
                .offered
                .into_iter()
                .find(|candidate| &candidate.profile_id == profile)
            {
                vec![(candidate.profile_id, candidate.choices)]
            } else {
                let discovery = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    backend.profile_config(profile.clone(), None, false),
                );
                let choices = await_or_cancel(cancelled, discovery)
                    .await?
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "review profile {profile:?} did not become available within 30 seconds"
                        )
                    })?
                    .map_err(|error| {
                        anyhow::anyhow!("review profile {profile:?} is unavailable: {error:#}")
                    })?;
                vec![(profile.clone(), choices)]
            }
        } else {
            let ready_models = ready
                .offered
                .iter()
                .flat_map(|candidate| candidate.choices.models.iter().cloned())
                .collect::<Vec<_>>();
            let candidates = if autoconfigure(&merged_model_offers(&ready_models), |_| None)
                .is_none()
            {
                await_or_cancel(cancelled, backend.reviewer_autoconfigure_candidates()).await??
            } else {
                ready
            };
            usage_priced.extend(usage_priced_profiles(&candidates));
            candidates
                .offered
                .into_iter()
                .map(|candidate| (candidate.profile_id, candidate.choices))
                .collect::<Vec<_>>()
        };
        let available = candidates;
        if let Some(profile) = pinned
            && available
                .iter()
                .all(|(_, choices)| choices.model.is_none() && choices.models.is_empty())
        {
            anyhow::bail!("review profile {profile:?} has no available reviewer model");
        }
        let (choices, _, _) = upgrade_reviewers(
            settings.legacy.as_ref().expect("legacy review"),
            &available,
            &usage_priced,
            |profile, model| {
                let backend = Arc::clone(backend);
                async move { backend.profile_config(profile, Some(model), false).await }
            },
            Some(cancelled),
        )
        .await?;
        choices
    } else {
        settings.reviewers
    };
    ensure!(
        !choices.is_empty(),
        "No reviewers are configured; choose reviewers or press Autoconfigure in Settings › Code Review."
    );
    Ok((choices, autoconfigured))
}

/// The profiles among `candidates` billed per use: their last quota report
/// says so, or their harness implies it (OpenCode reports no allowance, so a
/// profile on it has no subscription report). A profile whose billing neither
/// decides counts as a subscription here; see [`unclassified_in_draft`] for
/// callers that must not guess.
pub(crate) fn usage_priced_profiles(
    candidates: &crate::server::api::SubagentCandidates,
) -> BTreeSet<String> {
    use mj_client::quota::QuotaBilling;
    candidates
        .offered
        .iter()
        .filter(|candidate| candidate.quota.billing(candidate.harness) == QuotaBilling::Usage)
        .map(|candidate| candidate.profile_id.clone())
        .collect()
}

/// The profiles in `unclassified` (billing not yet known) that offer a model
/// in a family the legacy upgrade would draft from `available`. Empty when a
/// pinned profile decides the reviewers without Autoconfigure.
pub(crate) fn unclassified_in_draft(
    legacy: &LegacyReview,
    available: &[(String, ProfileConfig)],
    usage_priced: &BTreeSet<String>,
    unclassified: &BTreeSet<String>,
) -> Vec<String> {
    if unclassified.is_empty()
        || available
            .iter()
            .any(|(id, _)| legacy.profile.as_ref() == Some(id))
    {
        return Vec::new();
    }
    let offers = model_offers(
        available
            .iter()
            .map(|(profile, choices)| (choices.models.as_slice(), usage_priced.contains(profile))),
    );
    let Some(draft) = autoconfigure(&offers, |_| None) else {
        return Vec::new();
    };
    let all = available
        .iter()
        .flat_map(|(_, choices)| choices.models.iter().cloned())
        .collect::<Vec<_>>();
    available
        .iter()
        .filter(|(id, choices)| {
            unclassified.contains(id)
                && offers_drafted_family(&choices.models, &all, &draft.reviewers)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

pub(crate) async fn upgrade_reviewers<F, Fut>(
    legacy: &LegacyReview,
    available: &[(String, ProfileConfig)],
    usage_priced: &BTreeSet<String>,
    probe: F,
    cancelled: Option<&AtomicBool>,
) -> Result<(
    Vec<SubagentChoice>,
    bool,
    Vec<mj_core::review::autoconfigure::EffortAdjustment>,
)>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ProfileConfig>>,
{
    if let Some((_, choices)) = available
        .iter()
        .find(|(id, _)| legacy.profile.as_ref() == Some(id))
        && let Some(model) = choices
            .model
            .clone()
            .or_else(|| choices.models.first().map(|item| item.value.clone()))
    {
        let efforts = if choices.model.as_deref() == Some(&model) {
            Some(choices.efforts.clone())
        } else {
            let discovery = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                probe(legacy.profile.clone().unwrap(), model.clone()),
            );
            let result = match cancelled {
                Some(cancelled) => await_or_cancel(cancelled, discovery).await?,
                None => discovery.await,
            };
            result
                .ok()
                .and_then(Result::ok)
                .map(|choices| choices.efforts)
        };
        let effort = legacy.effort.as_ref().and_then(|requested| {
            efforts.as_ref().map_or_else(
                || Some(requested.clone()),
                |items| mj_core::review::autoconfigure::fallback_effort(requested, items),
            )
        });
        let adjustments = legacy
            .effort
            .as_ref()
            .filter(|requested| effort.as_ref() != Some(*requested))
            .map(
                |requested| mj_core::review::autoconfigure::EffortAdjustment {
                    model: model.clone(),
                    requested: requested.clone(),
                    used: effort.clone(),
                },
            )
            .into_iter()
            .collect();
        let count = legacy
            .count
            .map_or(2, |count| count.get())
            .clamp(1, mj_core::config::MAX_REVIEWERS);
        return Ok((
            vec![SubagentChoice { model, effort }; count],
            efforts.is_some(),
            adjustments,
        ));
    }
    let (choices, known) =
        autoconfigure_with_efforts(available, usage_priced, probe, cancelled).await?;
    Ok((
        choices.reviewers,
        known.values().all(|known| *known),
        choices.adjusted,
    ))
}

/// Compute the ordered legacy Auto preset using the same model and effort
/// discovery for the daemon upgrade and the startup review fallback.
pub(crate) async fn autoconfigure_with_efforts<F, Fut>(
    available: &[(String, ProfileConfig)],
    usage_priced: &BTreeSet<String>,
    probe: F,
    cancelled: Option<&AtomicBool>,
) -> Result<(Autoconfigured, BTreeMap<String, bool>)>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ProfileConfig>>,
{
    let offers = model_offers(
        available
            .iter()
            .map(|(profile, choices)| (choices.models.as_slice(), usage_priced.contains(profile))),
    );
    let draft = autoconfigure(&offers, |_| None)
        .unwrap_or_default()
        .reviewers;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut pairs = Vec::new();
    for model in draft
        .iter()
        .map(|choice| &choice.model)
        .collect::<BTreeSet<_>>()
    {
        for (profile, offered) in available {
            if offered.models.iter().any(|choice| &choice.value == model) {
                pairs.push((model.clone(), profile.clone(), offered));
            }
        }
    }
    let probe = &probe;
    let discovered = join_all(pairs.iter().map(|(model, profile, offered)| async move {
        if offered.model.as_deref() == Some(model) {
            return Ok(Some(offered.efforts.clone()));
        }
        let discovery = tokio::time::timeout_at(deadline, probe(profile.clone(), model.clone()));
        let result = match cancelled {
            Some(cancelled) => await_or_cancel(cancelled, discovery).await?,
            None => discovery.await,
        };
        Ok::<_, anyhow::Error>(match result {
            Ok(Ok(choices)) if choices.models.iter().any(|choice| &choice.value == model) => Some(choices.efforts),
            Ok(Ok(_)) => {
                tracing::warn!(%profile, %model, "model-specific reviewer discovery omitted the requested model");
                None
            }
            Ok(Err(error)) => {
                tracing::warn!(%profile, %model, %error, "reviewer effort discovery failed during legacy Auto configuration");
                None
            }
            Err(_) => {
                tracing::warn!(%profile, %model, "reviewer effort discovery timed out during legacy Auto configuration");
                None
            }
        })
    })).await;
    let mut efforts = BTreeMap::<String, Vec<mj_core::acp::SessionConfigChoice>>::new();
    let mut known = BTreeMap::<String, bool>::new();
    for ((model, _, _), result) in pairs.into_iter().zip(discovered) {
        let result = result?;
        let model_known = known.entry(model.clone()).or_insert(true);
        if let Some(items) = result {
            let union = efforts.entry(model).or_default();
            for item in items {
                if !union.iter().any(|existing| existing.value == item.value) {
                    union.push(item);
                }
            }
        } else {
            *model_known = false;
        }
    }
    let choices = autoconfigure(&offers, |model| {
        let complete = known.get(model) == Some(&true);
        let offered = efforts.get(model);
        if complete {
            return Some(offered.cloned().unwrap_or_default());
        }
        if offered.is_some_and(|offered| {
            draft
                .iter()
                .filter(|choice| choice.model == model)
                .all(|choice| {
                    choice
                        .effort
                        .as_ref()
                        .is_some_and(|effort| offered.iter().any(|item| &item.value == effort))
                })
        }) {
            offered.cloned()
        } else {
            None
        }
    })
    .unwrap_or_default();
    for adjustment in &choices.adjusted {
        tracing::warn!(model = %adjustment.model, requested = %adjustment.requested, used = ?adjustment.used, "legacy reviewer preset effort adjusted");
    }
    for choice in &choices.reviewers {
        if let Some(effort) = &choice.effort
            && efforts
                .get(&choice.model)
                .is_some_and(|items| items.iter().any(|item| &item.value == effort))
        {
            known.insert(choice.model.clone(), true);
        }
    }
    Ok((choices, known))
}

pub(crate) async fn await_or_cancel<T>(
    cancelled: &AtomicBool,
    work: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        value = work => Ok(value),
        _ = async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => anyhow::bail!("review preparation cancelled"),
    }
}

/// Whether a refusal only says another operation held the session: a lease
/// refused the reviewer action, a lease taken meanwhile cancelled it, or the
/// worker turned reviewer work away during a checkpoint or replacement (the
/// recovery copy a finished turn starts, or an upgrade to the current build).
pub(crate) fn preempted_by_lifecycle(reason: &str) -> bool {
    reason.contains("session is reserved for a lifecycle operation")
        || reason.contains("cancelled for session lifecycle change")
        || reason.contains("worker is reserved for checkpoint or replacement")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::acp::SessionConfigChoice;

    #[tokio::test]
    async fn legacy_named_profile_uses_its_default_model_and_validates_effort() {
        let legacy = LegacyReview {
            count: std::num::NonZeroUsize::new(3),
            profile: Some("claude".into()),
            effort: Some("xhigh".into()),
        };
        let available = vec![(
            "claude".into(),
            ProfileConfig {
                model: Some("haiku".into()),
                models: vec![SessionConfigChoice {
                    value: "haiku".into(),
                    name: "Haiku".into(),
                    description: None,
                }],
                efforts: vec![SessionConfigChoice {
                    value: "high".into(),
                    name: "High".into(),
                    description: None,
                }],
                observed_at: 1,
            },
        )];
        let (choices, known, adjusted) = upgrade_reviewers(
            &legacy,
            &available,
            &BTreeSet::new(),
            |_, _| async { unreachable!() },
            None,
        )
        .await
        .unwrap();
        assert!(known);
        assert_eq!(choices.len(), 3);
        assert_eq!(adjusted.len(), 1);
        assert_eq!(adjusted[0].requested, "xhigh");
        assert_eq!(adjusted[0].used.as_deref(), Some("high"));
        assert!(
            choices
                .iter()
                .all(|choice| choice.model == "haiku" && choice.effort.as_deref() == Some("high"))
        );
    }

    #[tokio::test]
    async fn legacy_named_profile_with_unknown_efforts_cannot_persist() {
        let legacy = LegacyReview {
            count: None,
            profile: Some("claude".into()),
            effort: Some("high".into()),
        };
        let available = vec![(
            "claude".into(),
            ProfileConfig {
                model: None,
                models: vec![SessionConfigChoice {
                    value: "haiku".into(),
                    name: "Haiku".into(),
                    description: None,
                }],
                efforts: Vec::new(),
                observed_at: 1,
            },
        )];
        let (choices, known, _) = upgrade_reviewers(
            &legacy,
            &available,
            &BTreeSet::new(),
            |_, _| async { anyhow::bail!("probe unavailable") },
            None,
        )
        .await
        .unwrap();
        assert!(!known);
        assert_eq!(choices.len(), 2);
        assert!(
            choices
                .iter()
                .all(|choice| choice.model == "haiku" && choice.effort.as_deref() == Some("high"))
        );
    }
    #[tokio::test]
    async fn legacy_auto_keeps_effort_offered_by_any_matching_profile() {
        let model = SessionConfigChoice {
            value: "claude-sonnet-4.5".into(),
            name: "Sonnet".into(),
            description: None,
        };
        let profile = |efforts: Vec<SessionConfigChoice>| ProfileConfig {
            model: Some(model.value.clone()),
            models: vec![model.clone()],
            efforts,
            observed_at: 1,
        };
        let available = vec![
            ("no-effort".into(), profile(Vec::new())),
            (
                "high-effort".into(),
                profile(vec![SessionConfigChoice {
                    value: "xhigh".into(),
                    name: "Extra high".into(),
                    description: None,
                }]),
            ),
        ];
        let (choices, known) = autoconfigure_with_efforts(
            &available,
            &BTreeSet::new(),
            |_, _| async { unreachable!() },
            None,
        )
        .await
        .unwrap();
        assert!(known.values().all(|known| *known));
        assert!(
            choices
                .reviewers
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("xhigh"))
        );
    }

    #[tokio::test]
    async fn legacy_auto_known_empty_efforts_use_default_and_remain_persistable() {
        let model = SessionConfigChoice {
            value: "claude-sonnet-4.5".into(),
            name: "Sonnet".into(),
            description: None,
        };
        let available = vec![(
            "sonnet".into(),
            ProfileConfig {
                model: Some(model.value.clone()),
                models: vec![model],
                efforts: Vec::new(),
                observed_at: 1,
            },
        )];
        let (configured, known) = autoconfigure_with_efforts(
            &available,
            &BTreeSet::new(),
            |_, _| async { unreachable!() },
            None,
        )
        .await
        .unwrap();
        assert_eq!(known.get("claude-sonnet-4.5"), Some(&true));
        assert!(
            configured
                .reviewers
                .iter()
                .all(|choice| choice.effort.is_none())
        );
        assert_eq!(configured.adjusted.len(), 2);
    }

    fn catalog_profile(models: &[&str]) -> ProfileConfig {
        ProfileConfig {
            model: None,
            models: models
                .iter()
                .map(|value| SessionConfigChoice {
                    value: (*value).into(),
                    name: if *value == "sonnet" {
                        "Sonnet 4.5"
                    } else {
                        value
                    }
                    .into(),
                    description: None,
                })
                .collect(),
            efforts: Vec::new(),
            observed_at: 1,
        }
    }

    /// Four codex and six claude subscriptions plus one usage-priced OpenCode
    /// profile that advertises the OpenRouter catalogue.
    fn live_catalog() -> Vec<(String, ProfileConfig)> {
        let codex =
            catalog_profile(&["gpt-6.1-terra", "gpt-6.1-sol", "gpt-6-sol", "gpt-5.6-terra"]);
        let claude = catalog_profile(&["sonnet", "claude-sonnet-4-5-20251001"]);
        let openrouter = catalog_profile(&[
            "openrouter/~openai/gpt-terra-latest",
            "openrouter/~openai/gpt-sol-latest",
            "openrouter/~anthropic/claude-sonnet-latest",
            "openrouter/anthropic/claude-sonnet-5.5",
            "openrouter/openai/gpt-5.6-terra",
        ]);
        let mut available = vec![("openrouter".to_owned(), openrouter)];
        available.extend((1..=4).map(|n| (format!("codex{n}"), codex.clone())));
        available.extend((1..=6).map(|n| (format!("claude{n}"), claude.clone())));
        available
    }

    fn picked(configured: &Autoconfigured) -> Vec<(&str, Option<&str>)> {
        configured
            .reviewers
            .iter()
            .map(|choice| (choice.model.as_str(), choice.effort.as_deref()))
            .collect()
    }

    #[test]
    fn only_unclassified_profiles_offering_a_drafted_family_block_the_upgrade() {
        let legacy = LegacyReview::default();
        let mut available = live_catalog();
        available.push((
            "other".to_owned(),
            catalog_profile(&["some-unrelated-model"]),
        ));
        let usage_priced = BTreeSet::from(["openrouter".to_owned()]);
        let blocked = |names: &[&str]| {
            unclassified_in_draft(
                &legacy,
                &available,
                &usage_priced,
                &names.iter().map(|name| (*name).to_owned()).collect(),
            )
        };
        assert!(blocked(&[]).is_empty());
        // Outside every drafted family: nothing to guess about.
        assert!(blocked(&["other"]).is_empty());
        assert_eq!(blocked(&["other", "claude3", "codex2"]), ["codex2"]);
        // Generic providers retain separate families: an unrelated provider's
        // unknown billing cannot delay the selected flagship's upgrade.
        let generic = vec![
            (
                "gemini".into(),
                catalog_profile(&["google/gemini-3-pro", "google/gemini-3.5-flash"]),
            ),
            ("glm".into(), catalog_profile(&["z-ai/glm-5.3"])),
        ];
        assert_eq!(
            unclassified_in_draft(
                &legacy,
                &generic,
                &BTreeSet::new(),
                &BTreeSet::from(["gemini".into(), "glm".into()])
            ),
            ["gemini"]
        );
        // A pinned profile decides without Autoconfigure.
        let pinned = LegacyReview {
            profile: Some("codex1".into()),
            ..LegacyReview::default()
        };
        assert!(
            unclassified_in_draft(
                &pinned,
                &available,
                &usage_priced,
                &BTreeSet::from(["claude3".to_owned()])
            )
            .is_empty()
        );
    }

    #[test]
    fn candidates_without_a_report_are_usage_priced_only_when_their_harness_says_so() {
        use crate::server::api::{CandidateQuota, SubagentCandidate, SubagentCandidates};
        use mj_core::config::HarnessKind;
        let candidate = |id: &str, harness, quota| SubagentCandidate {
            profile_id: id.to_owned(),
            harness,
            choices: catalog_profile(&["sonnet"]),
            quota,
            unhealthy: BTreeMap::new(),
        };
        let candidates = SubagentCandidates {
            offered: vec![
                candidate("opencode", HarnessKind::OpenCode, CandidateQuota::Unknown),
                candidate("dsh", HarnessKind::Dsh, CandidateQuota::Unknown),
                candidate("claude", HarnessKind::Claude, CandidateQuota::Unknown),
                candidate("api", HarnessKind::Codex, CandidateQuota::UsagePriced),
                candidate(
                    "codex",
                    HarnessKind::Codex,
                    CandidateQuota::Subscription {
                        windows: Vec::new(),
                        paid_credits: None,
                        refreshed_at_epoch_seconds: 0,
                    },
                ),
            ],
            unavailable: Vec::new(),
        };
        assert_eq!(
            usage_priced_profiles(&candidates),
            BTreeSet::from(["opencode".to_owned(), "dsh".to_owned(), "api".to_owned()])
        );
    }

    #[tokio::test]
    async fn usage_priced_openrouter_models_do_not_displace_subscription_reviewers() {
        let available = live_catalog();
        let usage_priced = BTreeSet::from(["openrouter".to_owned()]);
        let (configured, _) = autoconfigure_with_efforts(
            &available,
            &usage_priced,
            |_, model| async move { Ok(catalog_profile(&[&model])) },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            picked(&configured),
            [("gpt-6.1-terra", None), ("gpt-6.1-sol", None)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn auto_effort_probes_share_one_deadline() {
        let model = SessionConfigChoice {
            value: "claude-sonnet-4.5".into(),
            name: "Sonnet".into(),
            description: None,
        };
        let offered = ProfileConfig {
            model: None,
            models: vec![model],
            efforts: Vec::new(),
            observed_at: 1,
        };
        let available = vec![
            ("first".into(), offered.clone()),
            ("second".into(), offered),
        ];
        let start = tokio::time::Instant::now();
        let (choices, known) = autoconfigure_with_efforts(
            &available,
            &BTreeSet::new(),
            |_, _| async { std::future::pending::<anyhow::Result<ProfileConfig>>().await },
            None,
        )
        .await
        .unwrap();
        assert_eq!(choices.reviewers.len(), 2);
        assert_eq!(known.get("claude-sonnet-4.5"), Some(&false));
        assert_eq!(
            tokio::time::Instant::now().duration_since(start),
            std::time::Duration::from_secs(10)
        );
    }

    #[tokio::test]
    async fn cancelling_pending_reviewer_discovery_releases_preparation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiting = {
            let cancelled = Arc::clone(&cancelled);
            tokio::spawn(
                async move { await_or_cancel(&cancelled, std::future::pending::<()>()).await },
            )
        };
        tokio::task::yield_now().await;
        cancelled.store(true, Ordering::Release);
        let error = tokio::time::timeout(std::time::Duration::from_millis(250), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("review preparation cancelled"));
    }
}
