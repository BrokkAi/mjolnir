use super::*;
#[cfg(unix)]
use crate::controller::test_support::install_fake_command;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::{Arc, Mutex};

fn zai_profile(home: &Path, base_url: &str) -> HarnessProfile {
    std::fs::write(
        home.join("config.toml"),
        format!(
            "model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"{base_url}\"\n\
             env_key = \"ZAI_API_KEY\"\n\
             wire_api = \"responses\"\n"
        ),
    )
    .unwrap();
    HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.to_path_buf(),
        environment: [("ZAI_API_KEY".to_owned(), "coding-plan-key".to_owned())]
            .into_iter()
            .collect(),
        context_window_bytes: None,
        guardian_review_model: None,
    }
}

#[test]
fn a_custom_provider_profile_asks_its_provider_for_quota_not_chatgpt() {
    let home = tempfile::tempdir().unwrap();
    let request = QuotaRefreshRequest::for_profile(
        "glm",
        &zai_profile(home.path(), "https://api.z.ai/api/v1"),
        home.path().to_path_buf(),
    );
    assert_eq!(
        request.provider,
        Some(ProviderCredential {
            id: "zai".to_owned(),
            host: "api.z.ai".to_owned(),
            api_key: "coding-plan-key".to_owned(),
        })
    );
    assert!(crate::zai_usage::serves_quota(
        &request.provider.unwrap().host
    ));

    // A Codex profile that uses its own ChatGPT login keeps that path.
    let native = tempfile::tempdir().unwrap();
    let request = QuotaRefreshRequest::for_profile(
        "work",
        &HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: native.path().to_path_buf(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
        native.path().to_path_buf(),
    );
    assert_eq!(request.provider, None);
}

#[tokio::test]
async fn a_provider_without_a_quota_endpoint_reports_usage_pricing() {
    let home = tempfile::tempdir().unwrap();
    let request = QuotaRefreshRequest::for_profile(
        "other",
        &zai_profile(home.path(), "https://example.invalid/v1"),
        home.path().to_path_buf(),
    );
    let (outcome, _) = refresh_profile(request, None).await;
    assert_eq!(outcome.report.error, None);
    assert!(outcome.report.windows.is_empty());
    assert!(outcome.report.is_usage_priced());
    assert_eq!(outcome.report.compact(), API_LABEL);
}

#[test]
fn parses_kimi_summary_limits_and_booster_without_credentials() {
    let payload = serde_json::json!({
        "usage": {"name":"Weekly", "used":40, "limit":1000, "resetAt":"tomorrow"},
        "limits": [{"detail":{"remaining":"90", "limit":"100", "name":"5h"}}],
        "boosterWallet": {"balance":{"amountLeft":42000000}}
    });
    let (windows, extra) = parse_kimi_usage(&payload);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].used, Some(40));
    assert_eq!(windows[1].used, Some(10));
    assert_eq!(windows[0].label, "Week");
    assert_eq!(windows[0].remaining_percent, Some(96));
    assert_eq!(windows[1].label, "5H");
    assert_eq!(windows[1].remaining_percent, Some(90));
    assert_eq!(extra.as_deref(), Some("booster 42 remaining"));
}

#[test]
fn compact_includes_reset_and_error_states() {
    let report = ProfileQuota {
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![QuotaWindow {
            label: "5H".into(),
            remaining_percent: Some(70),
            used: None,
            limit: None,
            resets: Some("10:00 Jun 17".into()),
            resets_at_epoch_seconds: Some(14_400),
        }],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    };
    assert!(report.compact().contains("70% left"));
    assert!(report.compact().contains("resets 10:00 Jun 17"));
}

#[test]
fn compact_shows_login_expired_without_unavailable_prefix() {
    let report = ProfileQuota {
        profile_id: "claude2".into(),
        harness: HarnessKind::Claude,
        windows: vec![],
        extra: None,
        error: Some(claude_usage::LOGIN_EXPIRED.into()),
        refreshed_at_epoch_seconds: 0,
    };
    assert_eq!(report.compact(), claude_usage::LOGIN_EXPIRED);
    assert_eq!(
        report.error_label().as_deref(),
        Some(claude_usage::LOGIN_EXPIRED)
    );
}

#[test]
fn compact_shows_other_errors_as_unavailable() {
    let report = ProfileQuota {
        profile_id: "claude2".into(),
        harness: HarnessKind::Claude,
        windows: vec![],
        extra: None,
        error: Some("query Claude usage: HTTP 429".into()),
        refreshed_at_epoch_seconds: 0,
    };
    assert_eq!(report.compact(), "unavailable");
    assert_eq!(report.error_label().as_deref(), Some("unavailable"));
}

#[test]
fn compact_displays_a_shared_reset_once() {
    let report = ProfileQuota {
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        windows: vec![
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(70),
                used: None,
                limit: None,
                resets: Some("10:00 Jun 17".into()),
                resets_at_epoch_seconds: Some(14_400),
            },
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(55),
                used: None,
                limit: None,
                resets: Some("10:00 Jun 17".into()),
                resets_at_epoch_seconds: Some(14_400),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    };
    assert_eq!(
        report.compact(),
        "5H 70% left, resets 10:00 Jun 17 · Week 55% left"
    );
}

#[test]
fn compact_hides_claude_short_window_when_week_is_exhausted() {
    let report = ProfileQuota {
        profile_id: "claude".into(),
        harness: HarnessKind::Claude,
        windows: vec![
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(100),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            },
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(0),
                used: None,
                limit: None,
                resets: Some("03:59 Aug 14".into()),
                resets_at_epoch_seconds: None,
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 0,
    };

    assert_eq!(report.compact(), "Week 0% left, resets 03:59 Aug 14");
}

#[cfg(unix)]
#[tokio::test]
async fn a_grok_profile_reports_its_billing_period_as_one_quota_window() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("auth.json"), b"old credentials").unwrap();
    install_fake_command(
        directory.path(),
        "grok",
        "#!/bin/sh\nprintf 'refreshed credentials' > \"$GROK_HOME/auth.json\"\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *initialize*) printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n' ;;\n    *billing*) printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"config\":{\"creditUsagePercent\":25.0,\"currentPeriod\":{\"type\":\"USAGE_PERIOD_TYPE_WEEKLY\",\"end\":\"2026-08-18T05:22:07+00:00\"}},\"subscription_tier\":\"X Premium+\"}}\\n' ;;\n  esac\ndone\n",
    );
    let environment = BTreeMap::from([
        (
            "GROK_HOME".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        (
            "PATH".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
    ]);

    let (outcome, _) = refresh_profile(
        QuotaRefreshRequest {
            profile_id: "grok".into(),
            harness: HarnessKind::Grok,
            source_home: directory.path().to_path_buf(),
            environment,
            cwd: directory.path().to_path_buf(),
            provider: None,
        },
        None,
    )
    .await;
    assert!(outcome.credentials_changed);
    let report = outcome.report;

    assert_eq!(report.error, None, "{:?}", report.error);
    // One long window and no short one: Grok Build has no 5-hour budget.
    assert_eq!(report.windows.len(), 1);
    assert_eq!(report.weekly_window().unwrap().remaining_percent, Some(75));
    assert_eq!(report.five_hour_window(), None);
    // The subscription tier stays off the row; the fixture carries it to
    // prove it is ignored.
    assert_eq!(report.extra, None);
    assert!(report.compact().starts_with("Week 75% left, resets "));
}

/// A `codex app-server` stand-in on `PATH` that logs every request line it
/// reads, so a test can assert the exact protocol exchange.
#[cfg(unix)]
fn fake_codex_app_server(
    directory: &Path,
    script: &str,
) -> (BTreeMap<String, String>, std::path::PathBuf) {
    install_fake_command(directory, "codex", script);
    let log = directory.join("requests.jsonl");
    let environment = BTreeMap::from([
        ("PATH".to_owned(), directory.to_string_lossy().into_owned()),
        (
            "CODEX_USAGE_TEST_LOG".to_owned(),
            log.to_string_lossy().into_owned(),
        ),
        (
            "CODEX_AUTH_FILE".to_owned(),
            directory.join("auth.json").to_string_lossy().into_owned(),
        ),
    ]);
    (environment, log)
}

/// A Codex `auth.json` whose access token is a JWT expiring `expires_in`
/// from now, last refreshed `refreshed_ago` before now.
#[cfg(unix)]
fn write_codex_auth(home: &Path, expires_in: Duration, refreshed_ago: Duration) {
    use base64::Engine as _;

    let now = chrono::Utc::now();
    let expiry = (now + chrono::TimeDelta::from_std(expires_in).unwrap()).timestamp();
    let segment = |value: Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
    };
    let access_token = format!(
        "{}.{}.signature-is-never-checked",
        segment(serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
        segment(serde_json::json!({ "exp": expiry })),
    );
    let body = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": access_token,
            "refresh_token": "refresh",
            "id_token": "id",
            "account_id": "account",
        },
        "last_refresh": (now - chrono::TimeDelta::from_std(refreshed_ago).unwrap())
            .to_rfc3339(),
    });
    std::fs::write(home.join("auth.json"), serde_json::to_vec(&body).unwrap()).unwrap();
}

#[cfg(unix)]
fn codex_request_log(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}

#[cfg(unix)]
async fn poll_codex_profile(
    directory: &Path,
    environment: BTreeMap<String, String>,
) -> QuotaRefreshOutcome {
    let (outcome, client) = refresh_profile(
        QuotaRefreshRequest {
            profile_id: "codex".into(),
            harness: HarnessKind::Codex,
            source_home: directory.to_path_buf(),
            environment,
            cwd: directory.to_path_buf(),
            provider: None,
        },
        None,
    )
    .await;
    if let Some(client) = client {
        client.shutdown().await;
    }
    outcome
}

#[cfg(unix)]
#[tokio::test]
async fn a_codex_login_near_expiry_is_rotated_before_the_usage_query() {
    let directory = tempfile::tempdir().unwrap();
    // Ten minutes left on a one-hour token: inside the one-hour margin.
    write_codex_auth(
        directory.path(),
        Duration::from_secs(600),
        Duration::from_secs(3_000),
    );
    let (environment, log) = fake_codex_app_server(
        directory.path(),
        r#"#!/bin/sh
read_and_log() {
IFS= read -r line || exit 1
printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"auth_mode":"chatgpt","tokens":{"access_token":"rotated"}}' > "$CODEX_AUTH_FILE"
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
"#,
    );

    let outcome = poll_codex_profile(directory.path(), environment).await;

    assert_eq!(outcome.report.error, None);
    assert_eq!(
        outcome.report.five_hour_window().unwrap().remaining_percent,
        Some(75)
    );
    // The rotated file has to reach live sessions, which is what the
    // changed-credentials flag asks the daemon to do.
    assert!(outcome.credentials_changed);

    let messages = codex_request_log(&log);
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["method"], "initialize");
    assert_eq!(messages[1]["method"], "initialized");
    assert_eq!(messages[2]["method"], "account/read");
    assert_eq!(messages[2]["params"]["refreshToken"], true);
    assert_eq!(messages[3]["method"], "account/read");
    assert_eq!(messages[3]["params"]["refreshToken"], false);
    assert_eq!(messages[4]["method"], "account/rateLimits/read");
}

#[cfg(unix)]
#[tokio::test]
async fn a_codex_login_far_from_expiry_is_polled_without_a_rotation() {
    let directory = tempfile::tempdir().unwrap();
    // Ten hours left on an eleven-hour token: outside both margins.
    write_codex_auth(
        directory.path(),
        Duration::from_secs(10 * 3_600),
        Duration::from_secs(3_600),
    );
    let (environment, log) = fake_codex_app_server(
        directory.path(),
        r#"#!/bin/sh
read_and_log() {
IFS= read -r line || exit 1
printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
"#,
    );

    let outcome = poll_codex_profile(directory.path(), environment).await;

    assert_eq!(outcome.report.error, None);
    assert!(!outcome.credentials_changed);

    let messages = codex_request_log(&log);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["method"], "initialize");
    assert_eq!(messages[1]["method"], "initialized");
    assert_eq!(messages[2]["params"]["refreshToken"], false);
    assert_eq!(messages[3]["method"], "account/rateLimits/read");
}

#[cfg(unix)]
#[tokio::test]
async fn a_codex_app_server_without_the_refresh_flag_still_reports_quota() {
    let directory = tempfile::tempdir().unwrap();
    write_codex_auth(
        directory.path(),
        Duration::from_secs(600),
        Duration::from_secs(3_000),
    );
    let (environment, _log) = fake_codex_app_server(
        directory.path(),
        r#"#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"unknown parameter"}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":40,"windowDurationMins":300}}}}'
"#,
    );

    let outcome = poll_codex_profile(directory.path(), environment).await;

    assert_eq!(outcome.report.error, None);
    assert_eq!(
        outcome.report.five_hour_window().unwrap().remaining_percent,
        Some(60)
    );
}

#[test]
fn a_codex_refresh_margin_is_an_hour_or_a_tenth_of_the_token_life() {
    let hour = 3_600_000;
    let now = 1_800_000_000_000;
    // A short-lived token: the flat hour decides.
    assert!(codex_login_needs_refresh(
        Some(now + hour / 2),
        Some(now - hour / 2),
        now
    ));
    assert!(!codex_login_needs_refresh(
        Some(now + 2 * hour),
        Some(now - hour),
        now
    ));
    // A long-lived token: a tenth of its life is wider than the hour.
    assert!(codex_login_needs_refresh(
        Some(now + 3 * hour),
        Some(now - 40 * hour),
        now
    ));
    // Without a last refresh, only the flat hour is known.
    assert!(codex_login_needs_refresh(Some(now + hour / 2), None, now));
    assert!(!codex_login_needs_refresh(Some(now + 3 * hour), None, now));
    // An unreadable expiry is not a reason to spend the refresh token.
    assert!(!codex_login_needs_refresh(None, Some(now - hour), now));
}

#[tokio::test]
async fn a_missing_codex_credential_file_asks_for_no_rotation() {
    let directory = tempfile::tempdir().unwrap();
    assert!(!codex_login_is_near_expiry(&directory.path().join("auth.json")).await);
}

#[tokio::test]
async fn an_unreachable_grok_reports_the_failure_instead_of_a_zero_reading() {
    let directory = tempfile::tempdir().unwrap();

    let (outcome, _) = refresh_profile(
        QuotaRefreshRequest {
            profile_id: "grok".into(),
            harness: HarnessKind::Grok,
            source_home: directory.path().to_path_buf(),
            environment: BTreeMap::from([(
                "PATH".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            )]),
            cwd: directory.path().to_path_buf(),
            provider: None,
        },
        None,
    )
    .await;
    let report = outcome.report;

    assert!(report.windows.is_empty());
    assert_eq!(
        report.error.as_deref(),
        Some("Grok Build executable not found")
    );
}

#[tokio::test]
async fn muse_quota_refresh_recovers_and_populates_dashboard_windows() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = br#"{"providers":{"meta":{"access_token":"profile-token"}}}"#;
    std::fs::write(directory.path().join("auth.json"), credentials).unwrap();
    let rejected = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let app = Router::new()
        .route(
            "/muse-code/key",
            post(
                |State(rejected): State<Arc<std::sync::atomic::AtomicBool>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer profile-token");
                    assert_eq!(body, serde_json::json!({"onboard": false}));
                    if rejected.load(std::sync::atomic::Ordering::SeqCst) {
                        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "api_key": "must-not-be-persisted",
                            "subs_usage": {
                                "weekly": {"used_percent": 1, "resets_at": 1789344000},
                                "window": {
                                    "used_percent": 3,
                                    "window_duration_mins": 300,
                                    "resets_at": 1788890595
                                }
                            }
                        })),
                    )
                },
            ),
        )
        .with_state(rejected.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = QuotaRefreshRequest {
        profile_id: "muse".into(),
        harness: HarnessKind::Muse,
        source_home: directory.path().to_path_buf(),
        environment: BTreeMap::from([("TBH_MINT_BASE_URL".into(), format!("http://{address}"))]),
        cwd: directory.path().to_path_buf(),
        provider: None,
    };
    let mut manager = QuotaManager::default();
    manager
        .refresh_profiles(vec![request.clone()], |_| async {})
        .await;
    assert!(manager.reports()["muse"].error.is_some());
    rejected.store(false, std::sync::atomic::Ordering::SeqCst);
    manager
        .refresh_profiles(vec![request], |outcome| async move {
            assert!(!outcome.credentials_changed);
        })
        .await;
    let report = &manager.reports()["muse"];
    assert_eq!(report.error, None);
    assert_eq!(report.extra, None);
    assert_eq!(report.weekly_window().unwrap().remaining_percent, Some(99));
    assert_eq!(
        report.five_hour_window().unwrap().remaining_percent,
        Some(97)
    );
    assert_eq!(
        report.weekly_window().unwrap().resets_at_epoch_seconds,
        Some(1789344000)
    );
    assert!(report.weekly_window().unwrap().resets.is_some());
    assert!(report.compact().contains("Week 99% left"));
    assert_eq!(
        std::fs::read(directory.path().join("auth.json")).unwrap(),
        credentials
    );
    manager.shutdown().await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn expired_claude_credentials_report_login_expired() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-expired",
                "expiresAt": 1,
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let (outcome, _) = refresh_profile(
        QuotaRefreshRequest {
            profile_id: "claude2".into(),
            harness: HarnessKind::Claude,
            source_home: directory.path().to_path_buf(),
            environment: BTreeMap::new(),
            cwd: directory.path().to_path_buf(),
            provider: None,
        },
        None,
    )
    .await;
    let report = outcome.report;

    assert!(report.windows.is_empty());
    assert_eq!(report.error.as_deref(), Some(claude_usage::LOGIN_EXPIRED));
    assert_eq!(report.compact(), claude_usage::LOGIN_EXPIRED);
}

#[test]
fn a_monthly_window_shares_the_long_window_column_with_a_weekly_one() {
    for label in ["Week", "Month"] {
        let report = ProfileQuota {
            profile_id: "grok".into(),
            harness: HarnessKind::Grok,
            windows: vec![QuotaWindow {
                label: label.into(),
                remaining_percent: Some(60),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };

        assert!(report.weekly_window().is_some(), "{label}");
        assert_eq!(report.compact(), format!("{label} 60% left"));
    }
}

#[test]
fn kimi_uses_percent_left_and_hides_a_short_window_on_sustainable_pace() {
    let report = ProfileQuota {
        profile_id: "kimi".into(),
        harness: HarnessKind::Kimi,
        windows: vec![
            QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(94),
                used: Some(6),
                limit: Some(100),
                resets: Some("12:22 Aug 18".into()),
                resets_at_epoch_seconds: Some(604_800),
            },
            QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(97),
                used: Some(3),
                limit: Some(100),
                resets: Some("10:22 Aug 13".into()),
                resets_at_epoch_seconds: Some(18_000),
            },
        ],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: 3_600,
    };

    assert_eq!(report.compact(), "Week 94% left, resets 12:22 Aug 18");
}

#[test]
fn short_window_is_shown_only_when_burn_rate_projects_early_exhaustion() {
    let window = QuotaWindow {
        label: "5H".into(),
        remaining_percent: Some(70),
        used: None,
        limit: None,
        resets: Some("later".into()),
        resets_at_epoch_seconds: Some(14_400),
    };
    assert!(projects_exhaustion(&window, 0));

    let sustainable = QuotaWindow {
        remaining_percent: Some(80),
        ..window
    };
    assert!(!projects_exhaustion(&sustainable, 0));
}

#[derive(Clone, Default)]
struct KimiServerState {
    refresh_forms: Arc<Mutex<Vec<String>>>,
}

async fn test_kimi_usage(headers: HeaderMap) -> (StatusCode, Json<Value>) {
    let accepted = headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some("Bearer fresh-access");
    if accepted {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "usage": {"name": "Weekly", "used": 1, "limit": 100}
            })),
        )
    } else {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})))
    }
}

async fn test_kimi_refresh(State(state): State<KimiServerState>, body: Bytes) -> Json<Value> {
    state
        .refresh_forms
        .lock()
        .unwrap()
        .push(String::from_utf8(body.to_vec()).unwrap());
    Json(serde_json::json!({
        "access_token": "fresh-access",
        "refresh_token": "fresh-refresh",
        "expires_in": 900,
        "scope": "kimi-code",
        "token_type": "Bearer"
    }))
}

#[tokio::test]
async fn kimi_quota_refreshes_after_unauthorized_and_retries() {
    let state = KimiServerState::default();
    let app = Router::new()
        .route("/coding/v1/usages", get(test_kimi_usage))
        .route("/api/oauth/token", post(test_kimi_refresh))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let home = tempfile::tempdir().unwrap();
    let credentials_path = home.path().join("credentials/kimi-code.json");
    tokio::fs::create_dir_all(credentials_path.parent().unwrap())
        .await
        .unwrap();
    let future_expiry = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    tokio::fs::write(
        &credentials_path,
        serde_json::to_vec(&serde_json::json!({
            "access_token": "rejected-access",
            "refresh_token": "old-refresh",
            "expires_at": future_expiry,
            "scope": "kimi-code",
            "token_type": "Bearer",
            "expires_in": 900
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    let endpoint = format!("http://{address}");
    let environment = HashMap::from([
        ("KIMI_CODE_BASE_URL".into(), format!("{endpoint}/coding/v1")),
        ("KIMI_CODE_OAUTH_HOST".into(), endpoint),
    ]);

    let (windows, _) = query_kimi(home.path(), &environment).await.unwrap();

    assert_eq!(windows[0].used, Some(1));
    let form = {
        let forms = state.refresh_forms.lock().unwrap();
        assert_eq!(forms.len(), 1);
        url::form_urlencoded::parse(forms[0].as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>()
    };
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("old-refresh")
    );
    let saved = read_kimi_credentials(&credentials_path).await.unwrap();
    assert_eq!(saved.access_token, "fresh-access");
    assert_eq!(saved.refresh_token, "fresh-refresh");
    assert!(!home.path().join("oauth/kimi-code.lock").exists());
    server.abort();
}

/// Backdate the lock directory the way a holder that stopped heartbeating
/// leaves it behind.
fn age_kimi_lock(path: &Path, age: Duration) {
    touch_kimi_lock(path, SystemTime::now() - age).expect("backdate lock directory");
}

#[tokio::test]
async fn a_kimi_refresh_lock_left_by_a_crashed_holder_is_broken_and_reacquired() {
    let home = tempfile::tempdir().unwrap();
    let lock = home.path().join("oauth/kimi-code.lock");
    std::fs::create_dir_all(&lock).unwrap();
    age_kimi_lock(&lock, KIMI_LOCK_STALE_AFTER + Duration::from_secs(60));

    let started = std::time::Instant::now();
    let held = KimiRefreshLock::acquire(home.path(), Duration::from_secs(10))
        .await
        .expect("an orphaned lock must not block a refresh");
    let waited = started.elapsed();

    assert!(
        waited < Duration::from_secs(5),
        "acquisition waited {waited:?}"
    );
    drop(held);
    assert!(!lock.exists(), "the released lock must be gone");
}

#[tokio::test]
async fn a_heartbeating_kimi_refresh_lock_is_not_broken_by_a_waiter() {
    let home = tempfile::tempdir().unwrap();
    let lock = home.path().join("oauth/kimi-code.lock");
    std::fs::create_dir_all(&lock).unwrap();

    let error = KimiRefreshLock::acquire(home.path(), Duration::from_millis(600))
        .await
        .err()
        .expect("a lock with a live holder must be waited out, not stolen");

    assert!(
        error.to_string().contains("kimi-code.lock"),
        "the timeout must name the lock: {error}"
    );
    assert!(lock.exists(), "a live holder's lock must survive a waiter");
}

/// The Kimi Code CLI breaks a lock whose modification time is more than
/// five seconds old, so Mjolnir's beats have to be frequent enough that a
/// stalled heartbeat task still cannot cost it a live lock.
#[tokio::test]
async fn a_held_kimi_lock_republishes_its_mtime_several_times_per_cli_break_window() {
    let home = tempfile::tempdir().unwrap();
    let held = KimiRefreshLock::acquire(home.path(), KIMI_LOCK_WAIT)
        .await
        .unwrap();
    let lock = home.path().join("oauth/kimi-code.lock");

    // Half the peer's break window: two beats have to land inside it, so
    // Mjolnir publishes at least four times per window and can miss several in
    // a row and still hold the lock.
    const KIMI_CLI_LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
    let watched = KIMI_CLI_LOCK_STALE_AFTER / 2;
    let deadline = tokio::time::Instant::now() + watched;
    let mut published = vec![kimi_lock_mtime(&lock).unwrap().expect("the created lock")];
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let observed = kimi_lock_mtime(&lock).unwrap().expect("a held lock");
        if published.last() != Some(&observed) {
            published.push(observed);
        }
        assert_eq!(
            std::fs::read_dir(&lock).unwrap().count(),
            0,
            "the CLI releases this lock with a plain rmdir, so it must stay empty"
        );
    }

    assert!(
        published.len() >= 3,
        "the lock's modification time moved {} times in {watched:?}; the Kimi Code CLI breaks a lock after {KIMI_CLI_LOCK_STALE_AFTER:?} without a beat",
        published.len() - 1
    );
    drop(held);
}

fn process_is_gone(pid: i32) -> bool {
    // SAFETY: signal 0 only probes whether the process exists.
    unsafe { libc::kill(pid, 0) != 0 }
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_profile_from_the_configuration_stops_its_codex_quota_client() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("codex.pid");
    // A `codex app-server` stand-in: answer one quota refresh, then stay
    // alive on stdin the way the real one does between refreshes. The
    // dispatcher `exec`s this script, so `$$` is the spawned process.
    install_fake_command(
        directory.path(),
        "codex",
        r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_QUOTA_TEST_PID"
IFS= read -r line || exit 0
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 0
IFS= read -r line || exit 0
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 0
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
while IFS= read -r line; do :; done
"#,
    );
    let request = QuotaRefreshRequest {
        profile_id: "codex-1".into(),
        harness: HarnessKind::Codex,
        source_home: directory.path().to_path_buf(),
        environment: BTreeMap::from([
            (
                "PATH".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ),
            (
                "CODEX_QUOTA_TEST_PID".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            ),
        ]),
        cwd: directory.path().to_path_buf(),
        provider: None,
    };

    let mut quotas = QuotaManager::default();
    quotas.refresh_profiles(vec![request], |_| async {}).await;

    assert_eq!(
        quotas.reports()["codex-1"].error,
        None,
        "the stand-in app-server must answer the quota query"
    );
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(
        !process_is_gone(pid),
        "the app-server child is cached between refreshes"
    );

    // The profile leaves the configuration, so the next batch no longer
    // carries it.
    quotas.refresh_profiles(Vec::new(), |_| async {}).await;

    assert!(
        process_is_gone(pid),
        "a profile removed from the configuration must not leave its `codex app-server` child running"
    );
    quotas.shutdown().await;
}

#[test]
fn reset_time_normalization_uses_24_hour_month_day_format() {
    let paris = FixedOffset::east_opt(2 * 3_600).expect("offset");
    let reset = paris
        .with_ymd_and_hms(2026, 6, 17, 16, 49, 0)
        .single()
        .expect("instant");
    assert_eq!(format_reset_label(reset), "16:49 Jun 17");
    assert_eq!(
        normalize_reset_text("Jun 17 at 4:49pm").as_deref(),
        Some("16:49 Jun 17")
    );
}

#[test]
fn reset_timestamp_accepts_seconds_and_milliseconds() {
    let seconds = 1_781_712_540_f64;
    assert_eq!(
        format_reset_local(seconds),
        format_reset_local(seconds * 1_000.0)
    );
    assert_eq!(
        format_reset_local(seconds),
        format_reset_local_seconds(seconds as i64)
    );
}

#[test]
fn time_only_reset_is_rendered_as_the_next_datetime() {
    let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
    let now = zone
        .with_ymd_and_hms(2026, 8, 10, 14, 0, 0)
        .single()
        .expect("now");
    assert_eq!(
        normalize_reset_at("3:30 PM (America/Chicago)", now)
            .map(format_reset_label)
            .as_deref(),
        Some("15:30 Aug 10")
    );
    assert_eq!(
        normalize_reset_at("at 1pm (America/Chicago)", now)
            .map(format_reset_label)
            .as_deref(),
        Some("13:00 Aug 11")
    );
}

#[test]
fn claude_comma_separated_reset_is_normalized() {
    let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
    let now = zone
        .with_ymd_and_hms(2026, 8, 11, 7, 0, 0)
        .single()
        .expect("now");
    assert_eq!(
        normalize_reset_at("Aug 14, 4am (America/Chicago)", now)
            .map(format_reset_label)
            .as_deref(),
        Some("04:00 Aug 14")
    );
}
