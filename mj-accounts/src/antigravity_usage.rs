//! Antigravity's own structured `/usage` command, with an isolated login copy.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use mj_client::quota::{AntigravityQuotaPool, QuotaWindow};
use mj_core::config::{HarnessKind, harness_authentication_marker};
use serde_json::Value;

/// The environment the quota CLI is resolved and run under: the daemon's, then
/// the profile's, with the Antigravity home settings applied. The prewarm
/// resolves the managed runtime from this same environment.
pub(crate) fn runtime_environment(
    home: &Path,
    environment: &HashMap<String, String>,
) -> BTreeMap<String, String> {
    // Resolve the runtime from the daemon's environment before HOME is replaced
    // for the disposable login. Profile overrides retain their usual precedence.
    let mut inherited = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect::<BTreeMap<_, _>>();
    inherited.extend(environment.clone());
    let mut environment = inherited;
    HarnessKind::Antigravity.configure_home_environment(home, &mut environment);
    environment
}

/// The pinned `agy` a quota check runs. Unlike the other quota harnesses it
/// has no host-CLI step: the host is always served the managed installation.
#[cfg(unix)]
pub(crate) async fn managed_runtime(
    environment: &BTreeMap<String, String>,
) -> Result<mj_core::harness_install::Located> {
    let policy = mj_core::config::ExecutionPolicy::ConfiguredApprovals;
    let (command, args) =
        mj_core::harness_runtime::fallback_launch(HarnessKind::Antigravity, policy);
    mj_core::harness_install::locate(
        mj_core::placement::local_harness_runtime(),
        HarnessKind::Antigravity,
        policy,
        Path::new(&command),
        &args,
        environment,
    )
    .await
    .context("prepare Antigravity quota CLI")
}

pub async fn query(home: &Path, environment: &HashMap<String, String>) -> Result<Vec<QuotaWindow>> {
    // Only the managed runtime, which is Unix's, adds to the environment.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut environment = runtime_environment(home, environment);
    #[cfg(unix)]
    let located = managed_runtime(&environment).await?;
    // Retain Located until capture completes: its lease protects the running CLI.
    #[cfg(unix)]
    let program = {
        environment.extend(located.environment.clone());
        located
            .cli_entrypoint
            .as_ref()
            .context("Antigravity quota CLI was not resolved")?
    };
    #[cfg(not(unix))]
    let program = mj_core::program_path::find_antigravity_program(&environment)
        .await?
        .context("Antigravity requires agy installed on this target")?;
    let source = harness_authentication_marker(HarnessKind::Antigravity, home);
    let staged = mj_core::runtime::spawn_blocking(move || -> Result<tempfile::TempDir> {
        let staged = tempfile::tempdir().context("stage Antigravity quota login")?;
        let (_, bytes) =
            mj_core::credentials::read_credential_file(HarnessKind::Antigravity, &source)?;
        ensure!(!bytes.is_empty(), "Antigravity is not signed in");
        mj_core::credentials::write_credential_file(
            HarnessKind::Antigravity,
            &harness_authentication_marker(
                HarnessKind::Antigravity,
                &staged.path().join(".gemini"),
            ),
            &bytes,
        )?;
        Ok(staged)
    })
    .await
    .context("stage Antigravity quota login task failed")??;
    // --gemini-dir belongs to `agy acp`, not the top-level print command.
    // A temporary HOME selects the copied account on every supported host.
    let mut command = tokio::process::Command::new(program);
    command
        .args(["-p", "/usage", "--output-format", "json"])
        .envs(environment)
        .env("HOME", staged.path())
        .env("USERPROFILE", staged.path())
        .env("XDG_CONFIG_HOME", staged.path().join("config"))
        .env("XDG_CACHE_HOME", staged.path().join("cache"))
        .env("XDG_DATA_HOME", staged.path().join("data"))
        .current_dir(staged.path());
    let output = mj_core::subprocess::capture(&mut command)
        .await
        .context("run agy /usage")?;
    ensure!(
        output.status.success(),
        "agy /usage failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse(&output.stdout)
}

fn parse(bytes: &[u8]) -> Result<Vec<QuotaWindow>> {
    let response: Value = serde_json::from_slice(bytes).context("decode agy /usage JSON")?;
    ensure!(
        response.get("status").and_then(Value::as_str) == Some("SUCCESS"),
        "agy /usage refused the request: {}",
        response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no successful result")
    );
    ensure!(
        response.pointer("/command/name").and_then(Value::as_str) == Some("usage")
            && response.get("num_turns").and_then(Value::as_u64) == Some(0),
        "agy did not return the built-in quota command"
    );
    let groups = response
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .context("agy /usage reported no quota groups")?;
    struct Bucket<'a> {
        id: &'a str,
        pool: AntigravityQuotaPool,
        label: &'static str,
        duration_seconds: i64,
        percent: u8,
        reset: Option<i64>,
    }
    let mut buckets: Vec<Bucket> = Vec::new();
    for group in groups {
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .context("agy quota group has no buckets")?
        {
            let id = bucket
                .get("id")
                .and_then(Value::as_str)
                .context("agy quota bucket has no id")?;
            let Some(pool) = AntigravityQuotaPool::ALL
                .into_iter()
                .filter(|pool| id.starts_with(pool.bucket_prefix()))
                .max_by_key(|pool| pool.bucket_prefix().len())
            else {
                bail!("agy reported an unknown quota bucket: {id}");
            };
            // Google's /usage documentation defines a weekly and a five-hour
            // allowance for each group: https://antigravity.google/docs/cli/commands/usage/
            // Argon's group reports only the weekly one.
            let (label, duration_seconds) = match bucket.get("window").and_then(Value::as_str) {
                Some("weekly") => ("Week", 7 * 24 * 60 * 60),
                Some("5h") => ("5H", 5 * 60 * 60),
                window => bail!("agy reported an unknown quota window {window:?} for {id}"),
            };
            let remaining = bucket
                .get("remaining_fraction")
                .and_then(Value::as_f64)
                .context("agy quota bucket has no remaining fraction")?;
            ensure!(
                remaining.is_finite() && (0.0..=1.0).contains(&remaining),
                "invalid agy quota fraction for {id}"
            );
            // A pool nobody has used yet reports the Unix epoch: no window is
            // open, so there is no reset to wait for.
            let reset = bucket
                .get("reset_time")
                .and_then(Value::as_str)
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()
                .context("decode agy quota reset time")?
                .map(|at| at.timestamp())
                .filter(|at| *at > 0);
            // Only a provider-reported zero means the pool is exhausted.
            let percent = if remaining == 0.0 {
                0
            } else {
                (remaining * 100.0).round().max(1.0) as u8
            };
            if let Some(earlier) = buckets
                .iter()
                .find(|earlier| earlier.pool == pool && earlier.label == label)
            {
                bail!(
                    "agy reported two {label} allowances for its {pool:?} pool: {} and {id}",
                    earlier.id
                );
            }
            buckets.push(Bucket {
                id,
                pool,
                label,
                duration_seconds,
                percent,
                reset,
            });
        }
    }
    ensure!(!buckets.is_empty(), "agy /usage reported no quota windows");
    let separate_argon = buckets
        .iter()
        .any(|bucket| bucket.pool == AntigravityQuotaPool::Argon);
    Ok(buckets
        .into_iter()
        .map(|bucket| QuotaWindow {
            label: bucket.label.into(),
            remaining_percent: Some(bucket.percent),
            duration_seconds: Some(bucket.duration_seconds),
            model_scope: Some(bucket.pool.reported_scope(separate_argon).into()),
            used: None,
            limit: None,
            resets: bucket
                .reset
                .and_then(super::quota::format_reset_local_seconds),
            resets_at_epoch_seconds: bucket.reset,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from agy 1.5.11 on a Pro plan: Gemini, third-party (unused,
    /// so its resets are the Unix epoch) and a separate Gemini 4 Argon group.
    const RECORDED_USAGE: &[u8] = include_bytes!("../tests/fixtures/antigravity-usage.json");
    const GEMINI_GROUP: usize = 0;
    const THIRD_PARTY_GROUP: usize = 1;
    const ARGON_GROUP: usize = 2;
    const ARGON_ORCHESTRATOR: &str = "gemini-pro-orchestrator-g1-pro";

    fn recorded_usage() -> Value {
        serde_json::from_slice(RECORDED_USAGE).unwrap()
    }

    fn exhaust_group(response: &mut Value, group: usize) {
        for bucket in response
            .pointer_mut(&format!("/command/data/groups/{group}/buckets"))
            .unwrap()
            .as_array_mut()
            .unwrap()
        {
            bucket["remaining_fraction"] = serde_json::json!(0);
        }
    }

    fn parse_value(response: &Value) -> Vec<QuotaWindow> {
        parse(&serde_json::to_vec(response).unwrap()).unwrap()
    }

    fn now() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-11-20T08:00:00Z")
            .unwrap()
            .timestamp()
    }

    fn pool_windows(windows: &[QuotaWindow], pool: AntigravityQuotaPool) -> Vec<QuotaWindow> {
        windows
            .iter()
            .filter(|window| mj_client::quota::antigravity_pool_of_window(window) == Some(pool))
            .cloned()
            .collect()
    }

    #[test]
    fn recorded_usage_keeps_exhausted_third_party_quota_out_of_gemini_selection() {
        let mut response = recorded_usage();
        exhaust_group(&mut response, THIRD_PARTY_GROUP);
        let windows = parse_value(&response);
        let now = now();
        assert!(crate::quota::quota_score(&windows, "gemini-3.8-flash", now).unwrap() > 0.0);
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "gpt-oss-120b-medium",
        ] {
            assert_eq!(
                crate::quota::quota_score(&windows, model, now),
                Some(0.0),
                "{model}"
            );
        }
        assert_eq!(
            crate::quota::quota_score(&windows, "unrelated-model", now),
            None
        );

        let gemini_week = pool_windows(&windows, AntigravityQuotaPool::Gemini)
            .into_iter()
            .find(|window| window.label == "Week")
            .unwrap();
        let mut partial = pool_windows(&windows, AntigravityQuotaPool::Argon);
        for window in &mut partial {
            window.resets = None;
            window.resets_at_epoch_seconds = None;
        }
        crate::quota::merge_reset_windows(&mut partial, &windows);
        let restored = pool_windows(&partial, AntigravityQuotaPool::Argon);
        assert_eq!(restored.len(), 1, "{partial:?}");
        assert_eq!(
            restored[0].resets_at_epoch_seconds,
            pool_windows(&windows, AntigravityQuotaPool::Argon)[0].resets_at_epoch_seconds,
        );
        assert_ne!(
            restored[0].resets_at_epoch_seconds, gemini_week.resets_at_epoch_seconds,
            "a same-named Gemini window must not supply the Argon reset"
        );

        // Missing data must not become exhausted or full quota, and a small
        // positive remainder must not trigger an exhausted-pool Move.
        response
            .pointer_mut("/command/data/groups/0/buckets/0")
            .unwrap()["remaining_fraction"] = serde_json::json!(0.00001);
        let windows = parse(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(windows[0].remaining_percent, Some(1));
        response
            .pointer_mut("/command/data/groups/0/buckets/0")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("remaining_fraction");
        assert!(parse(&serde_json::to_vec(&response).unwrap()).is_err());
    }

    // Hard-won: 1791e5750: agy's separate Gemini 4 Argon bucket failed every Antigravity quota read.
    #[test]
    fn recorded_usage_scores_argon_against_its_own_pool_when_the_plan_keeps_it_apart() {
        let now = now();
        let windows = parse_value(&recorded_usage());
        let argon = pool_windows(&windows, AntigravityQuotaPool::Argon);
        assert_eq!(argon.len(), 1, "{windows:?}");
        assert!(argon[0].is_weekly_or_longer());
        // An unused pool reports the Unix epoch, which is no reset at all.
        for window in pool_windows(&windows, AntigravityQuotaPool::Claude) {
            assert_eq!(window.remaining_percent, Some(100));
            assert_eq!(window.resets_at_epoch_seconds, None, "{window:?}");
        }

        // Measured on this plan: an Argon turn draws on the Argon bucket
        // alone, and the Argon orchestrator also implements with Gemini.
        let mut gemini_spent = recorded_usage();
        exhaust_group(&mut gemini_spent, GEMINI_GROUP);
        let windows = parse_value(&gemini_spent);
        assert!(crate::quota::quota_score(&windows, "gemini-4-argon", now).unwrap() > 0.0);
        for model in ["gemini-4-neon", ARGON_ORCHESTRATOR] {
            assert_eq!(
                crate::quota::quota_score(&windows, model, now),
                Some(0.0),
                "{model}"
            );
        }
        let mut argon_spent = recorded_usage();
        exhaust_group(&mut argon_spent, ARGON_GROUP);
        let windows = parse_value(&argon_spent);
        assert!(crate::quota::quota_score(&windows, "gemini-4-neon", now).unwrap() > 0.0);
        for model in ["gemini-4-argon", ARGON_ORCHESTRATOR] {
            assert_eq!(
                crate::quota::quota_score(&windows, model, now),
                Some(0.0),
                "{model}"
            );
        }

        // A plan without a separate Argon group counts Argon in Gemini's.
        let mut shared = gemini_spent.clone();
        shared
            .pointer_mut("/command/data/groups")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .remove(ARGON_GROUP);
        let shared = parse_value(&shared);
        assert_eq!(
            crate::quota::quota_score(&shared, "gemini-4-argon", now),
            Some(0.0)
        );

        // A reading cached before Argon had its own pool carries Gemini's
        // other scope; it must refresh the same allowance, not return beside it.
        let mut fresh = parse_value(&gemini_spent);
        for window in &mut fresh {
            window.resets = None;
            window.resets_at_epoch_seconds = None;
        }
        crate::quota::merge_reset_windows(&mut fresh, &shared);
        let gemini = pool_windows(&fresh, AntigravityQuotaPool::Gemini);
        assert_eq!(gemini.len(), 2, "{fresh:?}");
        assert!(gemini.iter().all(|window| {
            window.model_scope.as_deref() == Some("gemini,!argon")
                && window.resets_at_epoch_seconds.is_some()
        }));
        assert!(crate::quota::quota_score(&fresh, "gemini-4-argon", now).unwrap() > 0.0);
    }

    #[tokio::test]
    #[ignore = "downloads pinned agy and needs a copied MJ_ANTIGRAVITY_TEST_HOME login"]
    async fn installed_antigravity_usage_reads_every_subscription_pool_without_a_turn() {
        let home =
            std::env::var_os("MJ_ANTIGRAVITY_TEST_HOME").expect("set a copied credential home");
        let cache = tempfile::tempdir().unwrap();
        let windows = query(
            Path::new(&home),
            &HashMap::from([(
                "XDG_CACHE_HOME".into(),
                cache.path().to_string_lossy().into_owned(),
            )]),
        )
        .await
        .unwrap();
        for pool in AntigravityQuotaPool::ALL {
            let scoped = pool_windows(&windows, pool);
            // Only plans that keep Argon apart report it, and only weekly.
            if pool == AntigravityQuotaPool::Argon && scoped.is_empty() {
                continue;
            }
            assert!(
                scoped.iter().any(|window| window.is_weekly_or_longer()),
                "{pool:?}: {windows:?}"
            );
            assert_eq!(
                scoped
                    .iter()
                    .any(|window| window.duration_seconds == Some(18_000)),
                pool != AntigravityQuotaPool::Argon,
                "{pool:?}: {windows:?}"
            );
            // An unused pool has no open window and so no reset.
            assert!(
                scoped
                    .iter()
                    .all(|window| window.remaining_percent.is_some()),
                "{pool:?}: {windows:?}"
            );
        }
    }
}
