use super::*;
use crate::config::harness_authentication_marker;

fn claude_credentials(expires_at: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "access",
            "refreshToken": "refresh",
            "expiresAt": expires_at,
            "refreshTokenExpiresAt": expires_at + 1_000,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
            "rateLimitTier": "default",
        }
    }))
    .unwrap()
}

/// `exp` of the access token every Codex fixture carries.
const CODEX_FIXTURE_EXPIRY_SECONDS: i64 = 1_785_901_860;

/// A JWT shaped like a Codex access token: header, `exp` claim, and a
/// signature nothing here verifies.
fn codex_access_token(expiry_seconds: i64) -> String {
    use base64::Engine as _;

    let segment = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
    };
    format!(
        "{}.{}.signature-is-never-checked",
        segment(serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
        segment(serde_json::json!({ "exp": expiry_seconds, "sub": "account" })),
    )
}

fn codex_credentials(last_refresh: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": codex_access_token(CODEX_FIXTURE_EXPIRY_SECONDS),
            "refresh_token": "refresh",
            "id_token": "id",
            "account_id": "account",
        },
        "last_refresh": last_refresh,
    }))
    .unwrap()
}

fn kimi_credentials(expires_at: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "access_token": "access",
        "refresh_token": "refresh",
        "expires_at": expires_at,
        "expires_in": 900,
        "scope": "all",
        "token_type": "Bearer",
    }))
    .unwrap()
}

fn grok_credentials(expiries: &[&str]) -> Vec<u8> {
    let grants = expiries
        .iter()
        .enumerate()
        .map(|(index, expires_at)| {
            (
                format!("https://auth.x.ai::grant-{index}"),
                serde_json::json!({
                    "key": "access",
                    "auth_mode": "oidc",
                    "refresh_token": "refresh",
                    "expires_at": expires_at,
                    "oidc_issuer": "https://auth.x.ai",
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::to_vec(&serde_json::Value::Object(grants)).unwrap()
}

fn snapshot(fingerprint: &str, freshness: Option<i64>) -> CredentialSnapshot {
    CredentialSnapshot {
        present: true,
        fingerprint: fingerprint.to_owned(),
        freshness_epoch_ms: freshness,
    }
}

#[test]
fn claude_freshness_reads_oauth_expiry_milliseconds() {
    assert_eq!(
        credential_freshness(HarnessKind::Claude, &claude_credentials(1_755_000_000_000)),
        Some(1_755_000_000_000)
    );
}

#[test]
fn codex_freshness_converts_last_refresh_to_milliseconds() {
    assert_eq!(
        credential_freshness(
            HarnessKind::Codex,
            &codex_credentials("2026-08-05T02:51:00.864587231Z")
        ),
        Some(1_785_898_260_864)
    );
}

#[test]
fn kimi_freshness_converts_expiry_seconds_to_milliseconds() {
    assert_eq!(
        credential_freshness(HarnessKind::Kimi, &kimi_credentials(1_755_000_000)),
        Some(1_755_000_000_000)
    );
}

#[test]
fn grok_freshness_reads_the_latest_rfc3339_grant_expiry() {
    assert_eq!(
        credential_freshness(
            HarnessKind::Grok,
            &grok_credentials(&["2026-08-17T02:19:01.724226598Z"])
        ),
        Some(1_786_933_141_724)
    );
    // A home may hold several grants; the newest expiry decides freshness.
    assert_eq!(
        credential_freshness(
            HarnessKind::Grok,
            &grok_credentials(&[
                "2026-08-17T02:19:01.724226598Z",
                "2026-08-17T04:19:01.724226598Z",
            ])
        ),
        Some(1_786_940_341_724)
    );
    // Non-UTC offsets normalize to the same instant.
    assert_eq!(
        credential_freshness(
            HarnessKind::Grok,
            &grok_credentials(&["2026-08-16T22:19:01.724226598-04:00"])
        ),
        Some(1_786_933_141_724)
    );
}

#[test]
fn every_harness_reports_freshness_from_its_own_credential_shape() {
    let fixtures = [
        (HarnessKind::Claude, claude_credentials(1_755_000_000_000)),
        (
            HarnessKind::Codex,
            codex_credentials("2026-08-05T02:51:00.864587231Z"),
        ),
        (HarnessKind::Kimi, kimi_credentials(1_755_000_000)),
        (
            HarnessKind::Grok,
            grok_credentials(&["2026-08-17T02:19:01.724226598Z"]),
        ),
    ];
    for kind in HarnessKind::ALL
        .into_iter()
        .filter(|kind| *kind != HarnessKind::Muse)
    {
        let (_, bytes) = fixtures
            .iter()
            .find(|(fixture, _)| *fixture == kind)
            .unwrap_or_else(|| panic!("{kind:?} needs a credential fixture"));
        assert!(
            credential_freshness(kind, bytes).is_some(),
            "{kind:?} freshness"
        );
    }
}

#[test]
fn every_harness_reports_expiry_only_where_hel_can_refresh_ahead_of_it() {
    let fixtures = [
        (
            HarnessKind::Claude,
            claude_credentials(1_755_000_000_000),
            Some(1_755_000_000_000),
        ),
        (
            HarnessKind::Codex,
            codex_credentials("2026-08-05T02:51:00.864587231Z"),
            Some(CODEX_FIXTURE_EXPIRY_SECONDS * 1_000),
        ),
        (
            HarnessKind::Kimi,
            kimi_credentials(1_755_000_000),
            Some(1_755_000_000_000),
        ),
        (
            HarnessKind::Grok,
            grok_credentials(&["2026-08-17T02:19:01.724226598Z"]),
            None,
        ),
        (HarnessKind::Muse, b"{}".to_vec(), None),
    ];
    for kind in HarnessKind::ALL {
        let (_, bytes, expected) = fixtures
            .iter()
            .find(|(fixture, _, _)| *fixture == kind)
            .unwrap_or_else(|| panic!("{kind:?} needs a credential fixture"));
        assert_eq!(credential_expiry(kind, bytes), *expected, "{kind:?} expiry");
    }
}

#[test]
fn codex_expiry_comes_from_the_access_token_rather_than_the_refresh_time() {
    // `last_refresh` orders two copies; only the token itself says when the
    // grant runs out, and the two are not the same number.
    let bytes = codex_credentials("2026-08-05T02:51:00.864587231Z");
    assert_eq!(
        credential_freshness(HarnessKind::Codex, &bytes),
        Some(1_785_898_260_864)
    );
    assert_eq!(
        credential_expiry(HarnessKind::Codex, &bytes),
        Some(CODEX_FIXTURE_EXPIRY_SECONDS * 1_000)
    );
}

#[test]
fn unreadable_access_tokens_report_no_expiry() {
    for access_token in [
        serde_json::Value::from("not-a-jwt"),
        serde_json::Value::from("header.not-base64!!.signature"),
        serde_json::Value::from(format!("header.{}.signature", {
            use base64::Engine as _;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"sub\":\"only\"}")
        })),
        serde_json::Value::Null,
    ] {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": access_token },
            "last_refresh": "2026-08-05T02:51:00.864587231Z",
        }))
        .unwrap();
        assert_eq!(credential_expiry(HarnessKind::Codex, &bytes), None);
    }
    assert_eq!(credential_expiry(HarnessKind::Codex, b"not json"), None);
    assert_eq!(credential_expiry(HarnessKind::Claude, b"{}"), None);
}

#[test]
fn unparseable_credentials_report_no_freshness() {
    for kind in HarnessKind::ALL {
        assert_eq!(credential_freshness(kind, b"not json"), None);
        assert_eq!(credential_freshness(kind, b"{}"), None);
    }
    assert_eq!(
        credential_freshness(HarnessKind::Codex, &codex_credentials("yesterday")),
        None
    );
}

#[test]
fn payload_validation_rejects_empty_oversized_and_malformed_documents() {
    assert!(validate_credential_payload(HarnessKind::Claude, b"").is_err());
    assert!(validate_credential_payload(HarnessKind::Claude, b"not json").is_err());
    assert!(
        validate_credential_payload(HarnessKind::Claude, &vec![b'a'; MAX_CREDENTIAL_BYTES + 1])
            .is_err()
    );
    assert!(validate_credential_payload(HarnessKind::Claude, &claude_credentials(1)).is_ok());
}

#[test]
fn identical_or_absent_copies_need_no_sync() {
    assert_eq!(
        reconcile(&CredentialSnapshot::absent(), &CredentialSnapshot::absent()),
        SyncAction::None
    );
    assert_eq!(
        reconcile(&snapshot("a", Some(2)), &snapshot("a", Some(1))),
        SyncAction::None
    );
}

#[test]
fn a_missing_side_takes_the_other_side_copy() {
    assert_eq!(
        reconcile(&snapshot("a", Some(1)), &CredentialSnapshot::absent()),
        SyncAction::Push
    );
    assert_eq!(
        reconcile(&CredentialSnapshot::absent(), &snapshot("b", Some(1))),
        SyncAction::Pull
    );
}

#[test]
fn the_fresher_copy_wins_and_a_known_time_beats_an_unknown_one() {
    assert_eq!(
        reconcile(&snapshot("a", Some(2)), &snapshot("b", Some(1))),
        SyncAction::Push
    );
    assert_eq!(
        reconcile(&snapshot("a", Some(1)), &snapshot("b", Some(2))),
        SyncAction::Pull
    );
    assert_eq!(
        reconcile(&snapshot("a", Some(1)), &snapshot("b", None)),
        SyncAction::Push
    );
    assert_eq!(
        reconcile(&snapshot("a", None), &snapshot("b", Some(1))),
        SyncAction::Pull
    );
}

#[test]
fn differing_copies_without_any_freshness_are_left_alone() {
    assert_eq!(
        reconcile(&snapshot("a", None), &snapshot("b", None)),
        SyncAction::None
    );
    assert_eq!(
        reconcile(&snapshot("a", Some(5)), &snapshot("b", Some(5))),
        SyncAction::None
    );
}

#[test]
fn canonical_write_is_owner_only_and_replaces_the_previous_file() {
    let home = tempfile::tempdir().unwrap();
    let path = harness_authentication_marker(HarnessKind::Kimi, home.path());
    write_credential_file(HarnessKind::Kimi, &path, &kimi_credentials(1)).unwrap();
    write_credential_file(HarnessKind::Kimi, &path, &kimi_credentials(2)).unwrap();
    let (snapshot, bytes) = read_credential_file(HarnessKind::Kimi, &path).unwrap();
    assert!(snapshot.present);
    assert_eq!(bytes, kimi_credentials(2));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn canonical_write_refuses_a_symlinked_destination() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = home.path().join("elsewhere.json");
    std::fs::write(&elsewhere, b"{}").unwrap();
    let path = home.path().join("auth.json");
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let error = write_credential_file(
        HarnessKind::Codex,
        &path,
        &codex_credentials("2026-01-01T00:00:00Z"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("symbolic link"));
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"{}");
}

#[test]
fn a_missing_credential_file_reads_as_an_absent_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let path = harness_authentication_marker(HarnessKind::Codex, home.path());
    let (snapshot, bytes) = read_credential_file(HarnessKind::Codex, &path).unwrap();
    assert!(!snapshot.present);
    assert!(bytes.is_empty());
}

#[test]
fn auth_failure_phrases_match_and_near_misses_do_not() {
    assert!(auth_failure_signature(
        HarnessKind::Claude,
        "Error: OAuth session expired and could not be refreshed"
    ));
    assert!(auth_failure_signature(
        HarnessKind::Codex,
        "{\"error\":{\"type\":\"authentication_error\"}}"
    ));
    assert!(auth_failure_signature(
        HarnessKind::Kimi,
        "invalid_grant: refresh token rejected"
    ));
    assert!(auth_failure_signature(
        HarnessKind::Kimi,
        "OAuthUnauthorizedError: The provided authorization grant is invalid"
    ));
    assert!(auth_failure_signature(
        HarnessKind::Codex,
        "error=INVALID_GRANT"
    ));
    assert!(!auth_failure_signature(
        HarnessKind::Claude,
        "the OAuth session expired last week, but we refreshed it"
    ));
    assert!(!auth_failure_signature(
        HarnessKind::Claude,
        "authentication succeeded"
    ));
    assert!(!auth_failure_signature(
        HarnessKind::Codex,
        "docs_src/authentication_error_status_code/tutorial001_an_py310.py"
    ));
    assert!(!auth_failure_signature(
        HarnessKind::Codex,
        "someauthentication_error"
    ));
    assert!(!auth_failure_signature(
        HarnessKind::Codex,
        "invalid_grant_result"
    ));
}

#[test]
fn only_harness_observations_request_credential_sync() {
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };

    let event = |observation| RelayEvent {
        format: crate::relay::RELAY_EVENT_FORMAT_V1,
        ordinal: 1,
        previous_digest: crate::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
        digest: "a".repeat(64),
        recorded_at_ms: 1,
        command_id: None,
        observation,
    };
    assert!(events_report_auth_failure(
        HarnessKind::Claude,
        &[event(RelayObservation::Warning {
            message: "OAuth session expired and could not be refreshed".into(),
        })]
    ));
    assert!(events_report_auth_failure(
        HarnessKind::Claude,
        &[event(RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from("Please run /login to continue"),
            ))),
        })]
    ));
    assert!(events_report_auth_failure(
        HarnessKind::Codex,
        &[event(RelayObservation::Warning {
            message: format!("{}: codex", crate::acp::PROMPT_AUTH_REQUIRED_MARKER),
        })]
    ));

    let observed_false_positive =
        "docs_src/authentication_error_status_code/tutorial001_an_py310.py";
    assert!(!events_report_auth_failure(
        HarnessKind::Codex,
        &[
            event(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    "call-1",
                    ToolCallUpdateFields::new().title(observed_false_positive),
                ))),
            }),
            event(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                    "call-2",
                    observed_false_positive,
                ))),
            }),
            event(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                    ContentBlock::from(observed_false_positive),
                ))),
            }),
            event(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::from("explain authentication_error"),
                ))),
            }),
            event(RelayObservation::TerminalOutput {
                terminal_id: "terminal-1".into(),
                output: observed_false_positive.into(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            }),
        ]
    ));
    assert!(!events_report_auth_failure(
        HarnessKind::Claude,
        &[event(RelayObservation::CommandInterrupted {
            command_id: "command-1".into(),
            command: crate::relay::RelayCommandKind::Prompt,
            message: "invalid_grant".into(),
        })]
    ));
    assert!(!events_report_auth_failure(
        HarnessKind::Claude,
        &[event(RelayObservation::CommandQueued {
            command_id: "command-1".into(),
            command: crate::relay::RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("explain invalid_grant")],
            },
            created_at_ms: 1,
        })]
    ));
    assert_eq!(
        relay_event_credential_sync_reason(&event(RelayObservation::Warning {
            message: crate::acp::PROMPT_EMPTY_RESPONSE_MARKER.into(),
        })),
        Some(CredentialSyncReason::EmptyPromptResponse)
    );
}

#[test]
fn login_commands_match_each_harness_cli() {
    let profile = |kind: HarnessKind| HarnessProfile {
        enabled: true,
        kind,
        home: PathBuf::from("/home/user/.config"),
        environment: Default::default(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let command = |kind: HarnessKind| login_command(&profile(kind)).expect("login command");
    assert_eq!(
        command(HarnessKind::Codex),
        ("codex".to_owned(), vec!["login".to_owned()])
    );
    assert_eq!(
        command(HarnessKind::Claude),
        (
            "claude".to_owned(),
            vec!["auth".to_owned(), "login".to_owned()]
        )
    );
    assert_eq!(
        command(HarnessKind::Kimi),
        ("kimi".to_owned(), vec!["login".to_owned()])
    );
    assert_eq!(
        command(HarnessKind::Grok),
        ("grok".to_owned(), vec!["login".to_owned()])
    );
}

#[test]
fn an_api_key_profile_reports_that_it_has_no_interactive_login() {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::write(
        home.path().join("config.toml"),
        "model_provider = \"zai\"\n\
         [model_providers.zai]\n\
         base_url = \"https://api.z.ai/api/v1\"\n\
         env_key = \"ZAI_API_KEY\"\n\
         wire_api = \"responses\"\n",
    )
    .expect("write config");
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: [("ZAI_API_KEY".to_owned(), "key".to_owned())]
            .into_iter()
            .collect(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let error = login_command(&profile)
        .expect_err("API-key profiles have no login")
        .to_string();
    assert!(error.contains("ZAI_API_KEY"), "{error}");
}

#[test]
fn a_queued_periodic_sync_is_not_queued_twice() {
    let mut queue = VecDeque::new();
    enqueue(
        &mut queue,
        SyncTrigger {
            profile_id: "work".into(),
            cause: None,
        },
    );
    enqueue(
        &mut queue,
        SyncTrigger {
            profile_id: "work".into(),
            cause: None,
        },
    );
    enqueue(
        &mut queue,
        SyncTrigger {
            profile_id: "work".into(),
            cause: Some(CredentialSyncCause {
                session_id: "session".into(),
                reason: CredentialSyncReason::EmptyPromptResponse,
            }),
        },
    );
    assert_eq!(queue.len(), 2);
    assert_eq!(
        queue[1]
            .cause
            .as_ref()
            .map(|cause| cause.session_id.as_str()),
        Some("session")
    );
}

#[cfg(unix)]
#[test]
fn github_tokens_are_validated_fingerprinted_and_removed_safely() {
    use std::os::unix::fs::PermissionsExt;

    assert!(validate_github_token(b"").is_err());
    assert!(validate_github_token(b"contains whitespace").is_err());
    assert!(validate_github_token(&vec![b'x'; MAX_GITHUB_TOKEN_BYTES + 1]).is_err());

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("github-token");
    let installed = write_github_token(&path, b"controller-token").unwrap();
    assert_eq!(installed, GithubTokenSnapshot::of("controller-token"));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (state, token) = read_github_token(&path).unwrap();
    assert_eq!(state, installed);
    assert_eq!(token.as_deref(), Some("controller-token"));

    remove_github_token(&path).unwrap();
    remove_github_token(&path).unwrap();
    assert_eq!(
        read_github_token(&path).unwrap().0,
        GithubTokenSnapshot::absent()
    );
}

#[cfg(unix)]
#[test]
fn claude_setup_tokens_round_trip_through_an_owner_only_profile_directory() {
    use std::os::unix::fs::PermissionsExt;

    assert!(validate_claude_oauth_token(b"").is_err());
    assert!(validate_claude_oauth_token(b"contains whitespace").is_err());
    assert!(validate_claude_oauth_token(&vec![b'x'; MAX_CLAUDE_OAUTH_TOKEN_BYTES + 1]).is_err());

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("profiles/claude");
    let path = directory.join("claude-oauth-token");
    assert_eq!(read_claude_oauth_token(&path).unwrap(), None);

    write_claude_oauth_token(&path, b"sk-ant-oat01-example\n").unwrap();

    assert_eq!(
        read_claude_oauth_token(&path).unwrap().as_deref(),
        Some("sk-ant-oat01-example")
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // Rotating the token replaces the file in place.
    write_claude_oauth_token(&path, b"sk-ant-oat01-second").unwrap();
    assert_eq!(
        read_claude_oauth_token(&path).unwrap().as_deref(),
        Some("sk-ant-oat01-second")
    );
}

#[cfg(unix)]
#[test]
fn claude_setup_token_reads_and_writes_refuse_symlink_destinations() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::write(&elsewhere, b"keep").unwrap();
    let path = root.path().join("claude-oauth-token");
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

    assert!(write_claude_oauth_token(&path, b"sk-ant-oat01-example").is_err());
    assert!(read_claude_oauth_token(&path).is_err());
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
}

#[test]
fn a_profile_setup_token_lives_beside_the_configuration_not_in_the_profile_home() {
    let path = claude_oauth_token_path("claude-max");
    assert!(path.ends_with("profiles/claude-max/claude-oauth-token"));
    assert!(path.starts_with(crate::config::config_dir()));
}

#[cfg(unix)]
#[test]
fn github_token_install_and_remove_refuse_symlink_destinations() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::write(&elsewhere, b"keep").unwrap();
    let path = root.path().join("github-token");
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

    assert!(write_github_token(&path, b"controller-token").is_err());
    assert!(remove_github_token(&path).is_err());
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
}
