//! Antigravity's own structured `/usage` command, with an isolated login copy.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use mj_client::quota::{AntigravityQuotaPool, QuotaWindow};
use mj_core::config::{HarnessKind, harness_authentication_marker};
use serde_json::Value;

pub(crate) async fn query(
    home: &Path,
    environment: &HashMap<String, String>,
) -> Result<Vec<QuotaWindow>> {
    let environment = environment.clone().into_iter().collect::<BTreeMap<_, _>>();
    let program = mj_core::harness_install::antigravity_program(&environment).await?;
    let source = harness_authentication_marker(HarnessKind::Antigravity, home);
    let staged = tokio::task::spawn_blocking(move || -> Result<tempfile::TempDir> {
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
    let mut windows = Vec::new();
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
            let Some((pool, period)) = AntigravityQuotaPool::ALL.into_iter().find_map(|pool| {
                id.strip_prefix(pool.bucket_prefix())
                    .map(|period| (pool, period))
            }) else {
                bail!("agy reported an unknown quota bucket: {id}");
            };
            // Google's /usage documentation defines a weekly and a five-hour
            // allowance for each group: https://antigravity.google/docs/cli/commands/usage/
            let (label, duration_seconds) = match period {
                "weekly" => ("Week", 7 * 24 * 60 * 60),
                "5h" => ("5H", 5 * 60 * 60),
                _ => bail!("agy reported an unknown quota window: {id}"),
            };
            let remaining = bucket
                .get("remaining_fraction")
                .and_then(Value::as_f64)
                .context("agy quota bucket has no remaining fraction")?;
            ensure!(
                remaining.is_finite() && (0.0..=1.0).contains(&remaining),
                "invalid agy quota fraction for {id}"
            );
            let reset = bucket
                .get("reset_time")
                .and_then(Value::as_str)
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()
                .context("decode agy quota reset time")?;
            // Only a provider-reported zero means the pool is exhausted.
            let percent = if remaining == 0.0 {
                0
            } else {
                (remaining * 100.0).round().max(1.0) as u8
            };
            ensure!(
                !windows.iter().any(|w: &QuotaWindow| w.label == label
                    && w.model_scope.as_deref() == Some(pool.model_scope())),
                "duplicate agy quota bucket: {id}"
            );
            windows.push(QuotaWindow {
                label: label.into(),
                remaining_percent: Some(percent),
                duration_seconds: Some(duration_seconds),
                model_scope: Some(pool.model_scope().into()),
                used: None,
                limit: None,
                resets: reset
                    .and_then(|at| super::quota::format_reset_local_seconds(at.timestamp())),
                resets_at_epoch_seconds: reset.map(|at| at.timestamp()),
            });
        }
    }
    ensure!(!windows.is_empty(), "agy /usage reported no quota windows");
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORDED_USAGE: &[u8] = include_bytes!("../tests/fixtures/antigravity-usage.json");

    #[test]
    fn recorded_usage_keeps_exhausted_third_party_quota_out_of_gemini_selection() {
        let mut response: Value = serde_json::from_slice(RECORDED_USAGE).unwrap();
        let buckets = response
            .pointer_mut("/command/data/groups/1/buckets")
            .unwrap()
            .as_array_mut()
            .unwrap();
        for bucket in buckets {
            bucket["remaining_fraction"] = serde_json::json!(0);
        }
        let windows = parse(&serde_json::to_vec(&response).unwrap()).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-12T16:00:00Z")
            .unwrap()
            .timestamp();
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

        let mut partial = windows
            .iter()
            .filter(|window| {
                AntigravityQuotaPool::of_window(window) == Some(AntigravityQuotaPool::Claude)
            })
            .cloned()
            .collect::<Vec<_>>();
        for window in &mut partial {
            window.resets = None;
            window.resets_at_epoch_seconds = None;
        }
        crate::quota::merge_reset_windows(&mut partial, &windows);
        let restored = partial
            .iter()
            .find(|window| {
                window.label == "Week"
                    && AntigravityQuotaPool::of_window(window) == Some(AntigravityQuotaPool::Claude)
            })
            .unwrap();
        assert_eq!(
            restored.resets_at_epoch_seconds, windows[2].resets_at_epoch_seconds,
            "a same-named Gemini window must not supply the Claude reset"
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

    #[tokio::test]
    #[ignore = "requires host-installed agy and a copied MJ_ANTIGRAVITY_TEST_HOME login"]
    async fn installed_antigravity_usage_reads_both_subscription_pools_without_a_turn() {
        let home =
            std::env::var_os("MJ_ANTIGRAVITY_TEST_HOME").expect("set a copied credential home");
        let windows = query(Path::new(&home), &HashMap::new()).await.unwrap();
        for pool in AntigravityQuotaPool::ALL {
            let scoped = windows
                .iter()
                .filter(|window| AntigravityQuotaPool::of_window(window) == Some(pool))
                .collect::<Vec<_>>();
            assert!(
                scoped.iter().any(|window| window.is_weekly_or_longer()),
                "{pool:?}: {windows:?}"
            );
            assert!(
                scoped
                    .iter()
                    .any(|window| window.duration_seconds == Some(18_000)),
                "{pool:?}: {windows:?}"
            );
            assert!(
                scoped
                    .iter()
                    .all(|window| window.remaining_percent.is_some()
                        && window.resets_at_epoch_seconds.is_some())
            );
        }
    }
}
