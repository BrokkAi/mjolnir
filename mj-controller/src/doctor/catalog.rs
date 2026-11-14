//! Whether each profile's advertised models suit how Mjolnir offers them.
//!
//! The advertised catalogue lives in the running daemon, which discovers it
//! once per profile. `mj doctor` itself only reads configuration, so the
//! caller supplies what the daemon reported and this module judges it.

use std::collections::BTreeMap;

use mj_core::config::{Config, HarnessProfile};
use mj_core::worker_launch::ProfileConfig;

use super::DoctorCheck;

/// A catalogue longer than this, without a `models` allow-list, floods model
/// pickers, sub-agent choices and review slots.
pub const LARGE_CATALOG: usize = 50;

/// What the daemon reported for each enabled profile, or why nothing could be
/// read.
pub enum Catalogs {
    /// No catalogue was read, for a reason that applies to every profile, such
    /// as no running daemon.
    NotChecked(String),
    /// Per profile id: the advertised catalogue, or why this profile's could
    /// not be read.
    Read(BTreeMap<String, Result<ProfileConfig, String>>),
}

/// The ids of the profiles whose catalogues the checks need.
pub fn catalog_profile_ids(config: &Config) -> Vec<String> {
    config
        .enabled_profiles()
        .map(|(id, _)| id.to_owned())
        .collect()
}

pub fn catalog_checks(config: &Config, catalogs: &Catalogs) -> Vec<DoctorCheck> {
    match catalogs {
        Catalogs::NotChecked(reason) => {
            if config.enabled_profiles().next().is_none() {
                return Vec::new();
            }
            vec![DoctorCheck::ready(
                "harness.catalog",
                "Profile model catalogues",
                format!("Not checked: {reason}"),
            )]
        }
        Catalogs::Read(read) => config
            .enabled_profiles()
            .map(|(id, profile)| match read.get(id) {
                Some(Ok(advertised)) => catalog_check(id, profile, advertised),
                Some(Err(reason)) => not_checked(id, reason),
                None => not_checked(id, "the daemon did not report it"),
            })
            .collect(),
    }
}

fn not_checked(id: &str, reason: &str) -> DoctorCheck {
    DoctorCheck::ready(
        format!("harness.{id}.catalog"),
        format!("Model catalogue of profile {id}"),
        format!(
            "Not checked: {reason}. Start Mjolnir and rerun `mj doctor` once its models are discovered."
        ),
    )
}

fn catalog_check(id: &str, profile: &HarnessProfile, advertised: &ProfileConfig) -> DoctorCheck {
    let check_id = format!("harness.{id}.catalog");
    let title = format!("Model catalogue of profile {id}");
    let count = advertised.models.len();
    let advertises = |value: &str| advertised.models.iter().any(|choice| choice.value == value);

    let unknown = profile
        .models
        .iter()
        .filter(|value| !advertises(value))
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "`models` lists {} that the profile does not advertise, so {} offered.",
                unknown.join(", "),
                if unknown.len() == 1 {
                    "it is not"
                } else {
                    "they are not"
                }
            ),
            format!(
                "Correct or remove the entry in [profiles.{id}] in config.toml. `mj models --profile {id} --unrestricted` lists the exact values the profile advertises."
            ),
        );
    }

    if let Some(default) = &advertised.model
        && !profile.models.is_empty()
        && !profile.models.contains(default)
    {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "The profile's current model {default:?} is not in `models`. It stays offered so sessions never run a model their picker hides."
            ),
            format!(
                "Add {default:?} to `models` in [profiles.{id}], or change the model the profile's harness starts with."
            ),
        );
    }

    if profile.models.is_empty() && count > LARGE_CATALOG {
        let openrouter = advertised
            .models
            .iter()
            .filter(|choice| choice.value.starts_with("openrouter/"))
            .count()
            * 2
            >= count;
        let example = advertised
            .model
            .as_deref()
            .or_else(|| {
                advertised
                    .models
                    .first()
                    .map(|choice| choice.value.as_str())
            })
            .unwrap_or("provider/model");
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "The profile advertises {count} models{} and has no `models` allow-list, so all of them fill model pickers, sub-agent choices, review slots and Autoconfigure.",
                if openrouter {
                    ", which looks like an OpenRouter catalogue"
                } else {
                    ""
                }
            ),
            format!(
                "Add a line such as `models = [{example:?}]` under [profiles.{id}] in config.toml, listing the models you want. `mj models --profile {id} --unrestricted` lists every value."
            ),
        );
    }

    let offered = if profile.models.is_empty() {
        "all offered".to_owned()
    } else {
        format!("{} offered through `models`", profile.models.len())
    };
    DoctorCheck::ready(
        check_id,
        title,
        format!("{count} models advertised, {offered}."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;
    use mj_core::acp::SessionConfigChoice;
    use mj_core::config::HarnessKind;

    fn profile(models: &[&str]) -> HarnessProfile {
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::OpenCode,
            home: "/profiles/ds8bit".into(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            muse_auto_review: None,
            claude_permission_mode: None,
            aws: None,
            codex_profile: None,
            models: models.iter().map(|value| (*value).to_owned()).collect(),
            legacy_subagents: Default::default(),
        }
    }

    fn advertised(default: &str, values: impl IntoIterator<Item = String>) -> ProfileConfig {
        ProfileConfig {
            model: Some(default.to_owned()),
            models: values
                .into_iter()
                .map(|value| SessionConfigChoice {
                    name: value.clone(),
                    value,
                    description: None,
                })
                .collect(),
            efforts: Vec::new(),
            observed_at: 1,
        }
    }

    fn check(models: &[&str], advertised: ProfileConfig) -> DoctorCheck {
        let config = Config {
            profiles: BTreeMap::from([("ds8bit".to_owned(), profile(models))]),
            ..Default::default()
        };
        let read = Catalogs::Read(BTreeMap::from([("ds8bit".to_owned(), Ok(advertised))]));
        let mut checks = catalog_checks(&config, &read);
        assert_eq!(checks.len(), 1);
        checks.remove(0)
    }

    fn openrouter(count: usize) -> ProfileConfig {
        advertised(
            "openrouter/vendor/model-0",
            (0..count).map(|index| format!("openrouter/vendor/model-{index}")),
        )
    }

    #[test]
    fn a_large_openrouter_catalogue_without_an_allow_list_warns_with_the_line_to_add() {
        let check = check(&[], openrouter(LARGE_CATALOG + 1));
        assert_eq!(check.status, CheckStatus::Warning);
        assert!(check.detail.contains("51 models"), "{}", check.detail);
        assert!(check.detail.contains("OpenRouter"), "{}", check.detail);
        let remediation = check.remediation.unwrap();
        assert!(
            remediation
                .contains(r#"`models = ["openrouter/vendor/model-0"]` under [profiles.ds8bit]"#),
            "{remediation}"
        );
    }

    #[test]
    fn a_catalogue_at_the_limit_or_with_an_allow_list_is_ready() {
        assert_eq!(
            check(&[], openrouter(LARGE_CATALOG)).status,
            CheckStatus::Ready
        );
        assert_eq!(
            check(&["openrouter/vendor/model-0"], openrouter(300)).status,
            CheckStatus::Ready
        );
    }

    #[test]
    fn an_entry_the_profile_does_not_advertise_warns() {
        let check = check(
            &["openrouter/vendor/model-0", "openrouter/vendor/typo"],
            openrouter(3),
        );
        assert_eq!(check.status, CheckStatus::Warning);
        assert!(
            check.detail.contains(r#""openrouter/vendor/typo""#),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("model-0"), "{}", check.detail);
    }

    #[test]
    fn a_current_model_outside_the_allow_list_warns() {
        let check = check(&["openrouter/vendor/model-1"], openrouter(3));
        assert_eq!(check.status, CheckStatus::Warning);
        assert!(
            check.detail.contains(r#""openrouter/vendor/model-0""#),
            "{}",
            check.detail
        );
    }

    #[test]
    fn a_catalogue_that_could_not_be_read_stays_ready() {
        let config = Config {
            profiles: BTreeMap::from([("ds8bit".to_owned(), profile(&[]))]),
            ..Default::default()
        };
        for catalogs in [
            Catalogs::NotChecked("no Mjolnir daemon is running".into()),
            Catalogs::Read(BTreeMap::from([(
                "ds8bit".to_owned(),
                Err("discovery is still running".to_owned()),
            )])),
        ] {
            let checks = catalog_checks(&config, &catalogs);
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].status, CheckStatus::Ready);
            assert!(checks[0].detail.contains("Not checked"), "{:?}", checks[0]);
        }
    }
}
