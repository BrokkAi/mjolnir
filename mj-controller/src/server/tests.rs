use super::*;
use std::collections::BTreeMap;
use std::path::Path;

use axum::http::Request;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use mj_core::config::{
    CONFIG_VERSION, ContainerTemplate, HarnessKind, HarnessProfile, PermissionMode, ProjectBundle,
    ProjectRepository, SshConnection,
};
use mj_core::state::{ProjectSourceIdentity, STATE_VERSION, SessionRecord};

#[test]
fn unified_tls_backends_use_the_selected_crypto_provider() {
    install_rustls_crypto_provider();

    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    let _builder = rustls::ServerConfig::builder();
}

#[test]
fn minted_desktop_cookie_validates_and_names_a_viewer() {
    let key = vec![7u8; COOKIE_KEY_BYTES];
    let value = mint_desktop_session_cookie(&key).unwrap();
    let viewer = cookie_viewer(&key, &value, now_unix());
    assert!(
        viewer.is_some(),
        "minted cookie must validate and carry a viewer id: {value:?}"
    );
    assert!(!session_cookie_valid(
        &[8u8; COOKIE_KEY_BYTES],
        &value,
        now_unix()
    ));
}

pub(super) fn sample_config_state() -> (Config, AppState) {
    let config = Config {
        keys: Default::default(),
        build_cache: Default::default(),
        subagents: Default::default(),
        version: CONFIG_VERSION,
        sessions_side: Default::default(),
        advanced: Default::default(),
        notify: Default::default(),
        show_stopped_sessions: false,
        spinner: Default::default(),
        theme: Default::default(),
        phone: Default::default(),
        continuation: Default::default(),
        review: Default::default(),
        sessionwiki: Default::default(),
        legacy_startup: (),
        machines: Default::default(),
        profiles: BTreeMap::from([(
            "codex-1".into(),
            HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                guardian_review_model: None,
                kind: HarnessKind::Codex,
                home: "/highly/secret/codex".into(),
                environment: BTreeMap::from([("GH_TOKEN".into(), "secret-token".into())]),
            },
        )]),
        bundles: BTreeMap::from([(
            "hel".into(),
            ProjectBundle {
                primary_repo: "hel".into(),
                repositories: vec![ProjectRepository {
                    id: "hel".into(),
                    github: Some("owner/hel".into()),
                    local: Some("/private/source/hel".into()),
                    destination: "hel".into(),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([
            (
                "podman".into(),
                TargetTemplate::LocalPodman {
                    container: ContainerTemplate {
                        build_cache: None,
                        image: "secret.registry/image".into(),
                        pull_policy: Default::default(),
                        platform: None,
                        cpus: None,
                        memory: None,
                        environment: BTreeMap::from([("TOKEN".into(), "secret-target".into())]),
                        workspace_storage: Default::default(),
                    },
                },
            ),
            ("raw".into(), TargetTemplate::LocalBare),
        ]),
    };
    let state = AppState {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(
            "session-1".into(),
            SessionRecord {
                build_cache: None,
                container_workspace: None,
                mjolnir_subagents: None,
                create_managed_worktree: None,
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                archived: false,
                container_cpus: None,
                container_memory: None,
                id: "session-1".into(),
                title: "Build Hel".into(),
                harness_kind: HarnessKind::Codex,
                last_profile: "codex-1".into(),
                bundle_id: "hel".into(),
                project_directory: None,
                managed_worktree: None,
                target_template_id: "podman".into(),
                resource_allocation: None,
                additional_mounts: vec![],
                state: SessionState::Running,
                target: None,
                native_session_id: Some("native-secret-id".into()),
                acp_session_title: Some("Build Hel".into()),
                session_title_override: None,
                created_at: "now".into(),
                updated_at: "now".into(),
                viewed_through_event_ordinal: 0,
                draft_input: String::new(),
                last_error: Some("secret-token at /highly/secret/codex".into()),
                last_checkpoint_error: None,
                checkpoint: None,
            },
        )]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    (config, state)
}

type TestServer = (
    Router,
    mpsc::Receiver<ControllerRequest>,
    mpsc::Receiver<ReadReceiptRequest>,
    mpsc::Receiver<PreflightRequest>,
    mpsc::Receiver<ClientStateRequest>,
);

fn app() -> TestServer {
    app_with_conversations(BTreeMap::new())
}

fn app_with_move_receiver() -> (Router, mpsc::Receiver<MovePreparationRequest>) {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    snapshot.sessions[0].capabilities.move_session = true;
    let (_snapshot_tx, snapshot_rx) = watch::channel(snapshot);
    let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
    let (action_tx, _action_rx) = mpsc::channel(8);
    let (bundle_tx, _bundle_rx) = mpsc::channel(8);
    let (receipt_tx, _receipt_rx) = mpsc::channel(8);
    let (preflight_tx, _preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, _client_state_rx) = mpsc::channel(8);
    let options = test_options(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
    .with_test_credentials("123456", b"01234567890123456789012345678901");
    (router(options), move_preparation_rx)
}

fn app_with_conversations(conversations: BTreeMap<String, BrowserTranscript>) -> TestServer {
    app_with(conversations, |_| {})
}

fn app_with_snapshot(adjust: impl FnOnce(&mut ViewerSnapshot)) -> TestServer {
    app_with(BTreeMap::new(), adjust)
}

fn app_with(
    conversations: BTreeMap<String, BrowserTranscript>,
    adjust: impl FnOnce(&mut ViewerSnapshot),
) -> TestServer {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    adjust(&mut snapshot);
    let (_snapshot_tx, snapshot_rx) = watch::channel(snapshot);
    let (_conversation_tx, conversation_rx) = watch::channel(conversations);
    let (action_tx, action_rx) = mpsc::channel(8);
    let (bundle_tx, _bundle_rx) = mpsc::channel(8);
    let (receipt_tx, receipt_rx) = mpsc::channel(8);
    let (preflight_tx, preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, client_state_rx) = mpsc::channel(8);
    let options = test_options(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
    .with_test_credentials("123456", b"01234567890123456789012345678901");
    (
        router(options),
        action_rx,
        receipt_rx,
        preflight_rx,
        client_state_rx,
    )
}

fn app_with_bundle_receiver() -> (Router, mpsc::Receiver<BundleRequest>) {
    let (config, state) = sample_config_state();
    let (_snapshot_tx, snapshot_rx) =
        watch::channel(ViewerSnapshot::from_config_state(&config, &state, 1));
    let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
    let (action_tx, _action_rx) = mpsc::channel(8);
    let (bundle_tx, bundle_rx) = mpsc::channel(8);
    let (receipt_tx, _receipt_rx) = mpsc::channel(8);
    let (preflight_tx, _preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, _client_state_rx) = mpsc::channel(8);
    let options = test_options(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
    .with_test_credentials("123456", b"01234567890123456789012345678901");
    (router(options), bundle_rx)
}

// Keep this test factory's arguments aligned with `ServerRequests`; each
// channel is asserted independently by the HTTP behavior tests below.
#[allow(clippy::too_many_arguments)]
fn test_options(
    snapshot_rx: watch::Receiver<ViewerSnapshot>,
    conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    action_tx: mpsc::Sender<ControllerRequest>,
    bundle_tx: mpsc::Sender<BundleRequest>,
    receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    preflight_tx: mpsc::Sender<PreflightRequest>,
    move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    client_state_tx: mpsc::Sender<ClientStateRequest>,
) -> ServerOptions {
    test_options_with_dictation(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
    .0
}

#[allow(clippy::too_many_arguments)]
fn test_options_with_dictation(
    snapshot_rx: watch::Receiver<ViewerSnapshot>,
    conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    action_tx: mpsc::Sender<ControllerRequest>,
    bundle_tx: mpsc::Sender<BundleRequest>,
    receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    preflight_tx: mpsc::Sender<PreflightRequest>,
    move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    client_state_tx: mpsc::Sender<ClientStateRequest>,
) -> (ServerOptions, mpsc::Receiver<DictationRequest>) {
    let (dictation_tx, dictation_rx) = mpsc::channel(8);
    let options = ServerOptions::new(
        "127.0.0.1:0".parse().unwrap(),
        snapshot_rx,
        conversation_rx,
        ServerRequests {
            action_tx,
            bundle_tx,
            receipt_tx,
            preflight_tx,
            move_preparation_tx,
            client_state_tx,
            dictation_tx,
        },
    )
    .unwrap();
    (options, dictation_rx)
}

fn app_with_dictation_receiver() -> (Router, mpsc::Receiver<DictationRequest>) {
    let (config, state) = sample_config_state();
    let (_snapshot_tx, snapshot_rx) =
        watch::channel(ViewerSnapshot::from_config_state(&config, &state, 1));
    let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
    let (action_tx, _action_rx) = mpsc::channel(8);
    let (bundle_tx, _bundle_rx) = mpsc::channel(8);
    let (receipt_tx, _receipt_rx) = mpsc::channel(8);
    let (preflight_tx, _preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, _client_state_rx) = mpsc::channel(8);
    let (options, dictation_rx) = test_options_with_dictation(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    );
    (
        router(options.with_test_credentials("123456", b"01234567890123456789012345678901")),
        dictation_rx,
    )
}

fn detached_options() -> ServerOptions {
    let (config, state) = sample_config_state();
    let (_snapshot_tx, snapshot_rx) =
        watch::channel(ViewerSnapshot::from_config_state(&config, &state, 1));
    let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
    let (action_tx, _action_rx) = mpsc::channel(1);
    let (bundle_tx, _bundle_rx) = mpsc::channel(1);
    let (receipt_tx, _receipt_rx) = mpsc::channel(1);
    let (preflight_tx, _preflight_rx) = mpsc::channel(1);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(1);
    let (client_state_tx, _client_state_rx) = mpsc::channel(1);
    test_options(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
}

/// A valid session cookie for the test server's key.
///
/// Most checks are about what an authenticated request does rather than
/// about how it authenticated, and going through the login route for each
/// one buys nothing.
fn cookie() -> String {
    format!(
        "{COOKIE_NAME}={}",
        signed_cookie_value(
            b"01234567890123456789012345678901",
            "test-viewer",
            now_unix().saturating_add(3600)
        )
    )
}

fn valid_wav() -> Bytes {
    let samples = vec![0_u8; 320];
    let mut wav = Vec::with_capacity(44 + samples.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36_u32 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&32_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);
    Bytes::from(wav)
}

async fn login_cookie(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/auth/session")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"code":"123456"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    response
        .headers()
        .get(SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn dictation_availability_requires_auth_and_forwards_typed_request() {
    let (app, mut requests) = app_with_dictation_receiver();
    let unauthorized = app
        .clone()
        .oneshot(
            Request::get("/api/sessions/session-1/dictation")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(requests.try_recv().is_err());

    let cookie = login_cookie(&app).await;
    let pending = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(
                Request::get("/api/sessions/session-1/dictation")
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = requests.recv().await.unwrap();
    assert_eq!(request.session_id, "session-1");
    assert!(matches!(
        request.operation,
        DictationOperation::Availability
    ));
    request
        .reply
        .send(Ok(DictationResponse::Availability {
            available: true,
            reason: None,
        }))
        .unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], br#"{"available":true}"#);
}

#[tokio::test]
async fn dictation_rejects_bad_wav_before_controller_dispatch() {
    let (app, mut requests) = app_with_dictation_receiver();
    let cookie = login_cookie(&app).await;
    let response = app
        .oneshot(
            Request::post("/api/sessions/session-1/dictation")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "audio/wav")
                .body(Body::from("not wav"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn dictation_rejects_a_third_upload_before_reading_its_body() {
    let (app, mut requests) = app_with_dictation_receiver();
    let cookie = login_cookie(&app).await;
    let request = || {
        Request::post("/api/sessions/session-1/dictation")
            .header(COOKIE, cookie.clone())
            .header(CONTENT_TYPE, "audio/wav")
            .body(Body::from(valid_wav()))
            .unwrap()
    };
    let first = tokio::spawn({
        let app = app.clone();
        let request = request();
        async move { app.oneshot(request).await.unwrap() }
    });
    let second = tokio::spawn({
        let app = app.clone();
        let request = request();
        async move { app.oneshot(request).await.unwrap() }
    });
    let first_request = requests.recv().await.unwrap();
    let second_request = requests.recv().await.unwrap();
    let third = app
        .oneshot(
            Request::post("/api/sessions/session-1/dictation")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "audio/wav")
                .body(Body::from_stream(futures::stream::poll_fn(
                    |_| -> std::task::Poll<Option<Result<Bytes, std::io::Error>>> {
                        panic!("overloaded dictation polled its body")
                    },
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
    first_request
        .reply
        .send(Ok(DictationResponse::Transcript {
            text: "first".into(),
        }))
        .unwrap();
    second_request
        .reply
        .send(Ok(DictationResponse::Transcript {
            text: "second".into(),
        }))
        .unwrap();
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    assert_eq!(second.await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn dictation_upload_rejects_unauthorized_missing_and_oversized_requests() {
    let (app, mut requests) = app_with_dictation_receiver();
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/sessions/session-1/dictation")
                .body(Body::from(valid_wav()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let cookie = login_cookie(&app).await;
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/sessions/missing/dictation")
                .header(COOKIE, &cookie)
                .body(Body::from(valid_wav()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Exercise the streamed body limit without relying on Content-Length.
    let response = app
        .oneshot(
            Request::post("/api/sessions/session-1/dictation")
                .header(COOKIE, cookie)
                .body(Body::from(vec![0_u8; MAX_AUDIO_BYTES + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(requests.try_recv().is_err());
}

fn app_with_background_stop_receiver(
    can_stop: bool,
) -> (Router, mpsc::Receiver<BackgroundTaskStopRequest>) {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    snapshot.sessions[0].background_tasks = vec![ViewerBackgroundTask {
        id: "terminal:background-1".into(),
        command: "cargo test".into(),
        started_at_ms: 1_000,
        can_stop,
    }];
    let (_snapshot_tx, snapshot_rx) = watch::channel(snapshot);
    let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
    let (action_tx, _action_rx) = mpsc::channel(8);
    let (bundle_tx, _bundle_rx) = mpsc::channel(8);
    let (receipt_tx, _receipt_rx) = mpsc::channel(8);
    let (preflight_tx, _preflight_rx) = mpsc::channel(8);
    let (move_preparation_tx, _move_preparation_rx) = mpsc::channel(8);
    let (client_state_tx, _client_state_rx) = mpsc::channel(8);
    let (stop_tx, stop_rx) = mpsc::channel(8);
    let mut options = test_options(
        snapshot_rx,
        conversation_rx,
        action_tx,
        bundle_tx,
        receipt_tx,
        preflight_tx,
        move_preparation_tx,
        client_state_tx,
    )
    .with_test_credentials("123456", b"01234567890123456789012345678901");
    options.set_background_task_stop_tx(stop_tx);
    (router(options), stop_rx)
}

#[tokio::test]
async fn background_task_stop_validates_the_snapshot_and_waits_for_acknowledgement() {
    let (app, mut requests) = app_with_background_stop_receiver(true);
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn({
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            app.oneshot(
                Request::post("/api/sessions/session-1/background-tasks/stop")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"background_task_id":"terminal:background-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = requests.recv().await.unwrap();
    assert_eq!(request.session_id, "session-1");
    assert_eq!(request.background_task_id, "terminal:background-1");
    request.reply.send(Ok(())).unwrap();
    assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);

    let (app, mut requests) = app_with_background_stop_receiver(false);
    let cookie = login_cookie(&app).await;
    let response = app
        .oneshot(
            Request::post("/api/sessions/session-1/background-tasks/stop")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"background_task_id":"terminal:background-1"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn background_task_stop_reports_provider_failure_without_leaking_details() {
    let (app, mut requests) = app_with_background_stop_receiver(true);
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post("/api/sessions/session-1/background-tasks/stop")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"background_task_id":"terminal:background-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = requests.recv().await.unwrap();
    request
        .reply
        .send(Err(BackgroundTaskStopFailure::Provider))
        .unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        &body[..],
        br#"{"error":"the provider could not stop this background task"}"#
    );
}

#[tokio::test]
async fn dictation_provider_failure_is_actionable_and_does_not_expose_details() {
    let (app, mut requests) = app_with_dictation_receiver();
    let cookie = login_cookie(&app).await;
    let pending = tokio::spawn(async move {
        app.oneshot(
            Request::post("/api/sessions/session-1/dictation")
                .header(COOKIE, cookie)
                .body(Body::from(valid_wav()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    let request = requests.recv().await.unwrap();
    request
        .reply
        .send(Err(DictationError::Provider(
            "private provider details".into(),
        )))
        .unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("transcription"));
    assert!(!body.contains("private provider details"));
}

#[tokio::test]
async fn dropped_dictation_handler_cancels_controller_request() {
    let (app, mut requests) = app_with_dictation_receiver();
    let cookie = login_cookie(&app).await;
    let pending = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post("/api/sessions/session-1/dictation")
                    .header(COOKIE, cookie)
                    .body(Body::from(valid_wav()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = requests.recv().await.unwrap();
    let cancel = request.cancel.clone();
    pending.abort();
    let _ = pending.await;
    assert!(cancel.is_cancelled());
    drop(request);
}

#[tokio::test]
async fn bundle_endpoint_authenticates_and_forwards_the_source() {
    let (app, mut bundles) = app_with_bundle_receiver();
    let unauthorized = app
        .clone()
        .oneshot(
            Request::post("/api/bundles")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"source":"example/app"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(bundles.try_recv().is_err());

    let cookie = login_cookie(&app).await;
    let response = tokio::spawn({
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            app.oneshot(
                Request::post("/api/bundles")
                    .header(CONTENT_TYPE, "application/json")
                    .header(COOKIE, cookie)
                    .body(Body::from(r#"{"source":"example/app"}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = bundles.recv().await.expect("bundle request forwarded");
    assert_eq!(request.source, "example/app");
    request.reply.send(Ok("app".into())).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), br#"{"bundle_id":"app"}"#);
}

#[tokio::test]
async fn bundle_endpoint_rejects_empty_and_oversized_sources_before_dispatch() {
    for source in [String::new(), "x".repeat(MAX_BUNDLE_SOURCE_CHARS + 1)] {
        let (app, mut bundles) = app_with_bundle_receiver();
        let cookie = login_cookie(&app).await;
        let response = app
            .oneshot(
                Request::post("/api/bundles")
                    .header(CONTENT_TYPE, "application/json")
                    .header(COOKIE, cookie)
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({"source": source})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(bundles.try_recv().is_err());
    }
}

#[tokio::test]
async fn bundle_endpoint_reports_invalid_source_as_a_client_error() {
    let (app, mut bundles) = app_with_bundle_receiver();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post("/api/bundles")
                    .header(CONTENT_TYPE, "application/json")
                    .header(COOKIE, cookie)
                    .body(Body::from(r#"{"source":"not a source"}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    });
    let request = bundles.recv().await.expect("bundle request forwarded");
    request
        .reply
        .send(Err(BundleFailure::InvalidSource))
        .unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("GitHub owner/repository"));
}

#[tokio::test]
async fn api_requires_a_valid_signed_cookie() {
    let (app, _, _, _, _) = app();
    let unauthorized = app
        .clone()
        .oneshot(Request::get("/api/snapshot").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let cookie = login_cookie(&app).await;
    let authorized = app
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
}

#[tokio::test]
async fn qr_login_exchanges_the_secret_for_a_cookie_and_redirects_cleanly() {
    let (app, _, _, _, _) = app();
    let rejected = app
        .clone()
        .oneshot(
            Request::get("/auth/login?token=wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let accepted = app
        .oneshot(
            Request::get("/auth/login?token=test-login-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
    assert_eq!(accepted.headers().get(LOCATION).unwrap(), "/");
    assert_eq!(accepted.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    assert!(accepted.headers().contains_key(SET_COOKIE));
}

#[test]
fn signed_cookie_rejects_expiry_and_tampering() {
    let key = b"01234567890123456789012345678901";
    let cookie = signed_cookie_value(key, "test-viewer", 200);
    assert!(session_cookie_valid(key, &cookie, 100));
    assert!(!session_cookie_valid(key, &cookie, 200));
    assert!(!session_cookie_valid(key, &format!("{cookie}x"), 100));
    assert!(!session_cookie_valid(b"another-key", &cookie, 100));
}

#[test]
fn generated_code_and_cookie_attributes_are_phone_safe() {
    let code = generate_viewer_code().unwrap();
    assert_eq!(code.len(), 6);
    assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
    let header = session_cookie_header("signed", Some(60), true)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(header.contains("HttpOnly"));
    assert!(header.contains("SameSite=Strict"));
    assert!(header.contains("Secure"));
    assert!(header.contains("Max-Age=60"));
}

#[test]
fn public_snapshot_omits_homes_environment_locators_and_raw_errors() {
    let (config, state) = sample_config_state();
    let json =
        serde_json::to_string(&ViewerSnapshot::from_config_state(&config, &state, 9)).unwrap();
    assert!(!json.contains("/highly/secret"));
    assert!(!json.contains("secret-token"));
    assert!(!json.contains("secret-target"));
    assert!(!json.contains("secret.registry"));
    assert!(!json.contains("native-secret-id"));
    assert!(json.contains("\"has_error\":true"));
}

#[test]
fn public_snapshot_keeps_running_sessions_but_omits_disabled_profiles() {
    let (mut config, state) = sample_config_state();
    config.profiles.get_mut("codex-1").unwrap().enabled = false;

    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 9);

    assert!(snapshot.profiles.is_empty());
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].profile_id, "codex-1");
}

#[test]
fn target_snapshot_uses_each_raw_host_project_history_and_leaves_managed_empty() {
    let (mut config, mut state) = sample_config_state();
    config
        .targets
        .insert("raw-local".into(), TargetTemplate::LocalBare);
    config.targets.insert(
        "raw-builder".into(),
        TargetTemplate::SshBare {
            ssh: SshConnection {
                host: "builder-a".into(),
                user: None,
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: PermissionMode::Guardian,
            workspace_prefix: "workspaces".into(),
        },
    );
    state.remember_project_directory("local", Path::new("/work/local"));
    state.remember_project_directory("builder-a", Path::new("/srv/builder"));
    state.remember_project_directory("other-host", Path::new("/not-published"));

    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let target = |id: &str| {
        snapshot
            .targets
            .iter()
            .find(|target| target.id == id)
            .unwrap()
    };
    assert_eq!(
        target("raw-local").recent_project_directories,
        vec!["/work/local"]
    );
    assert_eq!(
        target("raw-builder").recent_project_directories,
        vec!["/srv/builder"]
    );
    assert!(target("podman").recent_project_directories.is_empty());
}

#[test]
fn public_snapshot_exposes_only_review_status_configuration() {
    let (mut config, state) = sample_config_state();
    config.review = mj_core::config::ReviewConfig {
        enabled: true,
        tier: mj_core::review::lanes::ReviewTier::Extended,
        profile: Some("reviewer-1".into()),
        model: Some("private-review-model".into()),
        effort: Some("private-review-effort".into()),
    };

    let value =
        serde_json::to_value(ViewerSnapshot::from_config_state(&config, &state, 9)).unwrap();

    assert_eq!(
        value.get("review_config"),
        Some(&serde_json::json!({
            "enabled": true,
            "tier": "extended",
            "profile": "reviewer-1",
        }))
    );
    let json = value.to_string();
    assert!(!json.contains("private-review-model"));
    assert!(!json.contains("private-review-effort"));
}

fn sample_elicitation() -> ElicitationRequest {
    ElicitationRequest::from_acp_params(
        "elicitation-1",
        serde_json::json!({
            "sessionId": "session-1",
            "mode": "form",
            "message": "Which CI architecture should the workflow use?",
            "requestedSchema": {
                "type": "object",
                "required": ["question_0"],
                "properties": {
                    "question_0": {
                        "type": "string",
                        "title": "CI architecture",
                        "oneOf": [
                            {"const": "reusable", "title": "Reusable workflow"},
                            {"const": "matrix", "title": "Matrix job"}
                        ]
                    },
                    "question_0_custom": {
                        "type": "string",
                        "title": "Other",
                        "_meta": {"_askUserQuestionCustomAnswer": {
                            "questionId": "question_0",
                            "isCustomAnswer": true
                        }}
                    }
                }
            }
        }),
    )
    .expect("sample elicitation parses")
}

fn accept(pairs: &[(&str, &str)]) -> ElicitationResponse {
    ElicitationResponse::Accept {
        content: pairs
            .iter()
            .map(|(id, value)| {
                (
                    (*id).to_owned(),
                    mj_core::elicitation::ElicitationValue::String((*value).to_owned()),
                )
            })
            .collect(),
    }
}

fn pending_elicitation_snapshot(snapshot: &mut ViewerSnapshot) {
    snapshot.sessions[0].pending_elicitations = vec![sample_elicitation()];
}

#[tokio::test]
async fn elicitation_answer_is_typed_and_forwarded() {
    let (app, mut actions, _, _, _) = app_with_snapshot(pending_elicitation_snapshot);
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"respond-elicitation","session_id":"session-1","elicitation_id":"elicitation-1","response":{"action":"accept","content":{"question_0":"reusable"}}}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert_eq!(
        action.action,
        ControllerAction::RespondElicitation {
            session_id: "session-1".into(),
            elicitation_id: "elicitation-1".into(),
            response: accept(&[("question_0", "reusable")]),
        }
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn elicitation_answer_for_an_unknown_request_is_refused_without_reaching_the_controller() {
    let (app, mut actions, _, _, _) = app_with_snapshot(pending_elicitation_snapshot);
    let cookie = login_cookie(&app).await;
    let response = app
        .oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"respond-elicitation","session_id":"session-1","elicitation_id":"elicitation-9","response":{"action":"cancel"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(actions.try_recv().is_err());
}

#[test]
fn elicitation_answers_are_checked_against_the_request_the_agent_asked() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    pending_elicitation_snapshot(&mut snapshot);
    let respond = |response: ElicitationResponse| ControllerAction::RespondElicitation {
        session_id: "session-1".into(),
        elicitation_id: "elicitation-1".into(),
        response,
    };

    assert!(validate_action(&respond(accept(&[("question_0", "matrix")])), &snapshot).is_ok());
    // Declining and cancelling never carry content, so they are always
    // answerable.
    assert!(validate_action(&respond(ElicitationResponse::Decline), &snapshot).is_ok());
    // An option the agent never offered, a field it never published, and a
    // missing required answer are all refused.
    assert!(validate_action(&respond(accept(&[("question_0", "cron")])), &snapshot).is_err());
    assert!(validate_action(&respond(accept(&[("smuggled", "yes")])), &snapshot).is_err());
    assert!(validate_action(&respond(accept(&[])), &snapshot).is_err());
    // A custom answer stands in for the select it belongs to, exactly as
    // the chat form submits it.
    assert!(
        validate_action(
            &respond(accept(&[("question_0_custom", "a monorepo pipeline")])),
            &snapshot,
        )
        .is_ok()
    );
}

#[test]
fn oversized_elicitation_answers_are_refused() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    pending_elicitation_snapshot(&mut snapshot);
    let long = "x".repeat(MAX_ELICITATION_BYTES);
    assert!(
        validate_action(
            &ControllerAction::RespondElicitation {
                session_id: "session-1".into(),
                elicitation_id: "elicitation-1".into(),
                response: accept(&[("question_0_custom", long.as_str())]),
            },
            &snapshot,
        )
        .is_err()
    );
}

/// One slice of the browser application, named by the two markers that
/// bracket it in `src/web/viewer.js`.
///
/// Slicing keeps each check to the functions it is about, so an unrelated
/// change elsewhere in the application cannot make it fail for the wrong
/// reason. The markers are ordinary source text, so a rename that moves
/// them fails loudly here rather than silently testing nothing.
fn viewer_source(from: &str, to: &str) -> &'static str {
    let start = VIEWER_JS
        .find(from)
        .unwrap_or_else(|| panic!("src/web/viewer.js no longer contains {from:?}"));
    let end = VIEWER_JS[start..]
        .find(to)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("src/web/viewer.js no longer contains {to:?} after {from:?}"));
    &VIEWER_JS[start..end]
}

/// Run one JavaScript check under Node.
///
/// The check and the modules it imports are written to a real directory
/// rather than passed to `--eval`, so a failure reports a line number a
/// person can open, and so a check can import the shipped module under
/// test by its real name instead of against a copy pasted into a string.
fn run_web_check(name: &str, check: &str) {
    let directory = tempfile::tempdir().expect("temporary directory for a web check");
    for (file, source) in [
        ("test-dom.js", TEST_DOM_JS),
        ("markdown.js", MARKDOWN_JS),
        ("tool-output.js", TOOL_OUTPUT_JS),
    ] {
        std::fs::write(directory.path().join(file), source).expect("write a web module");
    }
    let path = directory.path().join(format!("{name}.mjs"));
    std::fs::write(&path, check).expect("write the web check");
    let output = std::process::Command::new("node")
        .arg(&path)
        .output()
        .expect("Node.js is required to exercise the web viewer");
    assert!(
        output.status.success(),
        "{name} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Run one JavaScript check that supplies its own environment, for the
/// checks that slice a function out of `viewer.js` and drive it against a
/// hand-written stub rather than importing a module.
fn run_viewer_script(name: &str, script: &str) {
    run_web_check(name, script);
}

#[test]
fn web_upgrade_waits_for_readiness_and_retries_only_explicit_refusals() {
    let source = viewer_source(
        "async function upgradeAwareFetch",
        "async function request(",
    );
    run_viewer_script(
        "upgrade-admission",
        &format!(
            r#"
{source}
const assert = (condition, message) => {{ if (!condition) throw new Error(message); }};
globalThis.setTimeout = callback => {{ callback(); }};
const options = {{ method: 'POST', body: JSON.stringify({{command_id:'steer-1', active_prompt_id:'turn-1'}}) }};
let actions = 0, probes = 0;
const response = (status, pending = false) => ({{ status, ok:status === 200,
  headers:{{get:() => pending ? 'pending' : null}}, body:{{cancel:async () => {{}}}} }});
globalThis.fetch = async (url, sent) => {{
  if (url === '/') {{
    probes++;
    if (probes === 1) throw new Error('old listener stopped');
    return response(200);
  }}
  assert(sent === options, 'request identity or steering target changed');
  return ++actions === 1 ? response(503, true) : response(200);
}};
assert((await upgradeAwareFetch('/api/actions', options)).ok, 'request did not complete');
assert(actions === 2 && probes === 2, 'handoff did not wait for the replacement');
globalThis.fetch = async () => {{ throw new Error('acknowledgement lost'); }};
let failed = false;
try {{ await upgradeAwareFetch('/api/actions', options); }} catch {{ failed = true; }}
assert(failed, 'an ambiguous mutation must not be replayed');
globalThis.fetch = async () => response(503);
assert((await upgradeAwareFetch('/api/actions', options)).status === 503,
  'unrelated service failures must not trigger an upgrade retry');
"#
        ),
    );
}

/// Live path suggestions are the only place the browser types and asks at
/// once, so the shipped source has to drop an answer that no longer matches
/// what the field holds, and accepting a row has to re-announce the edit.
#[test]
fn web_path_suggestions_drop_stale_answers_and_re_announce_an_accepted_row() {
    let source = viewer_source("function attachPathSuggestions(", "\nfunction pathField(");
    let setup = r#"
const PATH_SUGGESTION_DELAY_MS = 0;
const setTimeout = run => run();
const clearTimeout = () => {};
const el = (name, className, textContent) => ({
  tagName: name.toUpperCase(),
  className: className || '',
  textContent: textContent === undefined ? '' : textContent,
  dataset: {},
  children: [],
  attributes: {},
  classList: { add() {}, remove() {} },
  append(...children) { this.children.push(...children); },
  replaceChildren(...children) { this.children = children; },
  setAttribute(key, value) { this.attributes[key] = value; },
  addEventListener() {},
});
const requests = [];
let pending = null;
const request = (url, options) => new Promise(resolve => {
  requests.push({ url, body: JSON.parse(options.body) });
  pending = resolve;
});
class Event { constructor(type) { this.type = type; } }
const input = {
  value: '',
  listeners: new Map(),
  after(node) { this.next = node; },
  addEventListener(type, listener) {
    this.listeners.set(type, [...(this.listeners.get(type) || []), listener]);
  },
  dispatchEvent(event) { for (const l of this.listeners.get(event.type) || []) l(event); },
  fire(type, event = {}) { this.dispatchEvent({ type, preventDefault() {}, ...event }); },
};
const document = { activeElement: input };
const flush = () => new Promise(resolve => setImmediate(resolve));
"#;
    let checks = r#"
attachPathSuggestions(input, { host: () => 'raw', kind: 'directories', applies: () => true });
const list = input.next;

input.value = '/work/re';
input.fire('input');
if (requests.length !== 1) throw Error('the field did not ask for suggestions');
const answer = { candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: false };

// An answer for text the person has moved past is not drawn under them.
input.value = '/work/rep';
pending(answer);
await flush();
if (list.children.length !== 0) throw Error('a stale answer was rendered');

input.value = '/work/re';
input.fire('input');
pending(answer);
await flush();
if (list.children.length !== 2) throw Error('the answer was not rendered');
if (input.value !== '/work/re') throw Error('the typed text was rewritten');

input.fire('keydown', { key: 'ArrowDown' });
input.fire('keydown', { key: 'Enter' });
if (input.value !== '/work/repos/') throw Error('Enter did not accept the highlighted row');
if (requests.length !== 3 || requests[2].body.prefix !== '/work/repos/')
  throw Error('accepting did not re-announce the edit');
"#;
    run_viewer_script("path-suggestions", &format!("{setup}\n{source}\n{checks}"));
}

#[test]
fn web_configuration_repair_action_explains_missing_entries_without_a_request() {
    let source = viewer_source("async function runSessionAction(", "sessions.onclick");
    let setup = r#"
const pendingActions = new Set();
const pendingLifecycleActions = new Map();
const snapshot = { sessions: [{ id: 'broken', configuration_issue: 'Restore bundle project in config.toml' }] };
const errorNode = { textContent: '' };
"#;
    let checks = r#"
await runSessionAction({ action: 'repair-config', id: 'broken' }, errorNode);
if (!errorNode.textContent.includes('Restore bundle project')) throw Error('repair guidance missing');
snapshot.sessions[0].configuration_issue = null;
await runSessionAction({ action: 'repair-config', id: 'broken' }, errorNode);
if (!errorNode.textContent.includes('repaired')) throw Error('stale configuration diagnostic');
"#;
    run_viewer_script(
        "configuration-repair",
        &format!("{setup}\n{source}\n{checks}"),
    );
}

#[test]
fn viewer_reports_configuration_drift_without_exposing_private_configuration() {
    let (mut config, state) = sample_config_state();
    let bundle = config.bundles.remove("hel").unwrap();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert!(snapshot.sessions[0].has_error);
    assert!(
        snapshot.sessions[0]
            .configuration_issue
            .as_deref()
            .unwrap()
            .contains("missing bundle")
    );
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains("secret-token"));
    assert!(!json.contains("/highly/secret"));
    config.bundles.insert("hel".into(), bundle);
    let repaired = ViewerSnapshot::from_config_state(&config, &state, 2);
    assert!(repaired.sessions[0].configuration_issue.is_none());
}

#[test]
fn web_preflight_applies_resolved_path_and_ignores_cancelled_reply() {
    let source = viewer_source(
        "async function preflightNew()",
        "async function advanceNew()",
    );
    let setup = r#"
let newDraft = { targetId: 'remote', profileId: 'codex', bundleId: 'bundle', projectDirectory: '~/project', projectDirectories: {} };
let pendingNewPreflight = null, pendingNewPreflightController = null;
function targetIsBare() { return true; }
function renderNewForm() {}
function selectedWorkspaceId() { return 'workspace'; }
let resolveRequest;
function request() { return new Promise(resolve => { resolveRequest = resolve; }); }
"#;
    let checks = r#"
let pending = preflightNew();
resolveRequest({ project_directory: '/remote/project' });
if (!await pending || newDraft.projectDirectory !== '/remote/project' || newDraft.projectDirectories.remote !== '/remote/project') throw Error('resolved path was not applied');
newDraft.projectDirectory = '~/newer';
pending = preflightNew();
pendingNewPreflightController.abort();
resolveRequest({ project_directory: '/remote/stale' });
if (await pending || newDraft.projectDirectory !== '~/newer') throw Error('cancelled reply replaced draft');
"#;
    run_viewer_script("path-preflight", &format!("{setup}\n{source}\n{checks}"));
}

#[test]
fn embedded_viewer_displays_capacity_retry_deadlines() {
    let source = viewer_source(
        "function sessionActivityLabel(",
        "function updateSessionActivity(",
    );
    let setup = "const pendingLifecycleActions = new Map(); function isTransitioningSession() { return false; }";
    let checks = r#"
const session = { lifecycle: 'live', capacity_retry: { attempt: 2, retry_at_ms: 120000 } };
if (sessionActivityLabel(session, 60000) !== 'Model at capacity · retrying in 1m00s') throw Error('missing retry countdown');
if (sessionActivityLabel(session, 121000) !== 'Model at capacity · retrying in 0m00s') throw Error('negative retry countdown');
"#;
    run_viewer_script("capacity-retry", &format!("{setup}\n{source}\n{checks}"));
}

#[test]
fn embedded_viewer_lists_current_workspace_histories_and_retained_move_recovery() {
    let source = viewer_source("function isResumeSession(", "const resumeDrafts =");
    let setup = r#"
const snapshot = {
  sessions: [
{ id: "history-a", workspace_id: "workspace-a", capabilities: { resume: true } },
{ id: "history-b", workspace_id: "workspace-b", capabilities: { resume: true } },
{ id: "running-a", workspace_id: "workspace-a", lifecycle: "live", has_error: true, capabilities: { resume: false, open: false } },
{ id: "move-a", workspace_id: "workspace-a", capabilities: { resume: false }, move_recovery: { checkpoint_retained: true, phase: "failed" } },
{ id: "moving-a", workspace_id: "workspace-a", capabilities: { resume: false }, move_recovery: { checkpoint_retained: true, phase: "starting_queue" } },
  ],
};
function selectedWorkspaceId() { return "workspace-a"; }
function sessionActivityMs() { return 0; }
function epochMs() { return null; }
"#;
    let checks = r#"
const ids = workspace => resumeSessions(workspace).map(session => session.id).sort();
if (JSON.stringify(ids("workspace-a")) !== JSON.stringify(["history-a", "move-a"])) {
  throw new Error(`workspace A histories or recoveries were wrong: ${JSON.stringify(ids("workspace-a"))}`);
}
if (JSON.stringify(ids("workspace-b")) !== JSON.stringify(["history-b"])) {
  throw new Error(`workspace B histories were wrong: ${JSON.stringify(ids("workspace-b"))}`);
}
if (ids("missing-workspace").length !== 0) throw new Error("unknown workspace exposed sessions");
"#;
    run_viewer_script(
        "workspace-resume-history",
        &format!("{setup}\n{source}\n{checks}"),
    );
}

#[test]
fn embedded_viewer_sends_the_selected_resume_workspace() {
    let source = viewer_source("async function runSessionAction", "sessions.onclick =");
    let setup = r#"
const pendingActions = new Set();
const pendingLifecycleActions = new Map();
const snapshot = { sessions: [] };
let sent = null;
function selectedWorkspaceId() { return "workspace-b"; }
function navigate() {}
function renderRoute() {}
async function refresh() {}
async function request(path, options) {
  sent = { path, body: JSON.parse(options.body) };
}
"#;
    let checks = r#"
const errorNode = { textContent: "" };
await runSessionAction(
  { action: "resume", id: "history-a", profile: "codex-1", target: "podman" },
  errorNode,
  { queue: "start" },
);
if (sent.path !== "/api/actions" || sent.body.workspace_id !== "workspace-b") {
  throw new Error(`resume did not carry its destination: ${JSON.stringify(sent)}`);
}
"#;
    run_viewer_script(
        "resume-workspace-destination",
        &format!("{setup}\n{source}\n{checks}"),
    );
}

#[test]
fn embedded_viewer_warns_before_stopping_an_active_session() {
    let source = viewer_source("async function runSessionAction", "sessions.onclick =");
    let setup = r#"
const pendingActions = new Set();
const pendingLifecycleActions = new Map();
const snapshot = {
  sessions: [
{ id: "active", chat_phase: "running" },
{ id: "idle", chat_phase: "idle" },
  ],
};
const questions = [];
function confirm(question) { questions.push(question); return false; }
function navigate() {}
"#;
    let checks = r#"
const errorNode = { textContent: "" };
await runSessionAction({ action: "suspend", id: "active" }, errorNode);
await runSessionAction({ action: "suspend", id: "idle" }, errorNode);
if (!questions[0].startsWith("Suspend session?\n\n")) {
  throw new Error(`active close warning was ${JSON.stringify(questions[0])}`);
}
if (!questions[0].includes("current turn will be interrupted")) {
  throw new Error(`active close omitted interruption: ${JSON.stringify(questions[0])}`);
}
if (!questions[1].startsWith("Suspend session?\n\n")) {
  throw new Error(`idle close warning was ${JSON.stringify(questions[1])}`);
}
"#;
    run_viewer_script(
        "active-session-stop-confirmation",
        &format!("{setup}\n{source}\n{checks}"),
    );
}

/// The projection publishes what the browser needs to group and filter
/// without publishing what the redaction contract keeps back. A project
/// key groups two sessions in one project together and says nothing about
/// where that project lives.
#[test]
fn the_project_key_groups_without_naming_a_path() {
    let (config, mut state) = sample_config_state();
    let first = state.sessions["session-1"].clone();
    let mut second = first.clone();
    second.id = "session-2".into();
    state.sessions.insert(second.id.clone(), second);
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);

    let keys = snapshot
        .sessions
        .iter()
        .map(|session| session.project_key.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 1, "two sessions in one project did not group");
    let key = keys.into_iter().next().expect("one key");
    assert!(!key.is_empty(), "the project key is empty");
    assert!(
        !key.contains('/') && !key.contains("hel"),
        "the project key leaks its identity: {key}"
    );
    assert_eq!(
        snapshot.sessions[0].project_label, "hel",
        "the project label should be a name a person recognises"
    );
}

#[test]
fn web_project_keys_follow_the_complete_repository_set() {
    let (mut config, mut state) = sample_config_state();
    let shared_bundle = config.bundles["hel"].clone();
    config.bundles.insert("other".into(), shared_bundle);

    let mut other = state.sessions["session-1"].clone();
    other.id = "session-2".into();
    other.bundle_id = "other".into();
    state.sessions.insert(other.id.clone(), other);

    assert_eq!(
        config.bundles["hel"].primary_repo,
        config.bundles["other"].primary_repo
    );
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let first = snapshot
        .sessions
        .iter()
        .find(|session| session.id == "session-1")
        .expect("first session");
    let second = snapshot
        .sessions
        .iter()
        .find(|session| session.id == "session-2")
        .expect("second session");

    assert_eq!(first.project_label, "hel");
    assert_eq!(second.project_label, "hel");
    assert_eq!(first.project_key, second.project_key);

    let secondary = ProjectRepository {
        id: "secondary".into(),
        github: Some("owner/secondary".into()),
        local: None,
        destination: "secondary".into(),
        git_ref: None,
    };
    config
        .bundles
        .get_mut("other")
        .unwrap()
        .repositories
        .push(secondary.clone());
    let project_keys = |config: &Config| {
        ViewerSnapshot::from_config_state(config, &state, 1)
            .sessions
            .into_iter()
            .map(|session| session.project_key)
            .collect::<Vec<_>>()
    };
    let keys = project_keys(&config);
    assert_ne!(
        keys[0], keys[1],
        "an added repository must change the bundle identity"
    );

    let first_bundle = config.bundles.get_mut("hel").unwrap();
    first_bundle.repositories.insert(0, secondary);
    first_bundle.primary_repo = "secondary".into();
    let keys = project_keys(&config);
    assert_eq!(
        keys[0], keys[1],
        "the same repository set must group together despite order or primary choice"
    );
}

#[test]
fn viewer_session_applies_a_resolved_source_without_publishing_it() {
    let (config, state) = sample_config_state();
    let mut viewer = ViewerSnapshot::from_config_state(&config, &state, 1)
        .sessions
        .into_iter()
        .next()
        .expect("session");
    let source = ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git")
        .expect("GitHub source");

    viewer.set_project_source(&source);

    assert_eq!(viewer.project_label, "bifrost-dev");
    assert_eq!(viewer.project_key, project_key(&source.key));
    let json = serde_json::to_string(&viewer).expect("serialize viewer session");
    assert!(!json.contains("BrokkAi"));
    assert!(!json.contains("github.com"));
}

/// A phone groups and filters by the lifecycle category, so the mapping
/// from the controller's precise state has to be the controller's own.
#[test]
fn lifecycle_categories_decide_what_the_dashboard_shows() {
    use ViewerLifecycleCategory::{Failed, Live, Starting, Suspended, Suspending};

    for (state, expected, on_dashboard) in [
        (SessionState::Provisioning, Starting, true),
        (SessionState::Running, Live, true),
        (SessionState::Disconnected, Live, true),
        (SessionState::Checkpointing, Live, true),
        (SessionState::Closing, Suspending, true),
        (SessionState::Destroying, Suspending, true),
        (SessionState::Stopped, Suspended, false),
        (SessionState::Lost, Failed, false),
        (SessionState::Error, Failed, false),
        (SessionState::DestroyedWithDataLoss, Failed, false),
    ] {
        let category = ViewerLifecycleCategory::of(state);
        assert_eq!(category, expected, "{state:?}");
        assert_eq!(
            category.is_dashboard_visible(),
            on_dashboard,
            "{state:?} belongs on the dashboard? "
        );
    }
}

/// Resume compatibility travels as the set the browser can offer, so it
/// never has to subtract one list from another and never offers a target
/// the controller would refuse.
#[test]
fn compatible_resume_targets_are_the_complement_of_the_incompatible_ones() {
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let session = &snapshot.sessions[0];
    let all = config.targets.keys().cloned().collect::<Vec<_>>();

    for target in &all {
        assert_ne!(
            session.compatible_resume_targets.contains(target),
            session.incompatible_resume_targets.contains(target),
            "target {target} is in both lists or neither"
        );
    }
    assert_eq!(
        session.compatible_resume_targets.len() + session.incompatible_resume_targets.len(),
        all.len(),
        "the two lists do not cover every target"
    );
}

/// The viewer renders a control because a capability says so. An action
/// whose capability is false is refused at the boundary, so a forged
/// request gets the same answer a well-behaved viewer would never ask for.
#[tokio::test]
async fn actions_are_refused_when_their_capability_is_false() {
    for (body, capability) in [
        (
            r#"{"action":"interrupt-turn","session_id":"session-1"}"#,
            "cancel_turn",
        ),
        (
            r#"{"action":"set-plan-mode","session_id":"session-1","active":true}"#,
            "set_plan_mode",
        ),
        (
            r#"{"action":"set-config","session_id":"session-1","key":"model","value":"x"}"#,
            "set_config",
        ),
    ] {
        let (app, mut actions, _, _, _) = app();
        let response = post_action(app, cookie(), body.to_owned()).await;
        assert!(
            response.status().is_client_error(),
            "{capability} was accepted while false: {}",
            response.status()
        );
        assert!(
            actions.try_recv().is_err(),
            "{capability} reached the controller while false"
        );
    }
}

/// A setting the harness never advertised is not a setting. Forwarding one
/// asks the agent to refuse something the viewer should never have offered.
#[tokio::test]
async fn a_config_key_the_harness_never_advertised_is_refused() {
    let capable = |snapshot: &mut ViewerSnapshot| {
        snapshot.sessions[0].capabilities.set_config = true;
        snapshot.sessions[0].config_options = vec![ViewerConfigOption {
            key: "model".into(),
            label: "model".into(),
            current: None,
            choices: vec![ViewerConfigChoice {
                value: "sonnet".into(),
                name: "Sonnet".into(),
                description: None,
            }],
        }];
    };

    for (body, why) in [
        (
            r#"{"action":"set-config","session_id":"session-1","key":"effort","value":"high"}"#,
            "an unadvertised key",
        ),
        (
            r#"{"action":"set-config","session_id":"session-1","key":"model","value":"gpt-9"}"#,
            "an unoffered value",
        ),
    ] {
        let (app, mut actions, _, _, _) = app_with_snapshot(capable);
        let response = post_action(app, cookie(), body.to_owned()).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{why} was accepted"
        );
        assert!(actions.try_recv().is_err(), "{why} reached the controller");
    }

    // The value the harness did advertise is forwarded unchanged.
    let (app, mut actions, _, _, _) = app_with_snapshot(capable);
    let response = tokio::spawn(post_action(
        app,
        cookie(),
        r#"{"action":"set-config","session_id":"session-1","key":"model","value":"sonnet"}"#
            .to_owned(),
    ));
    let action = actions
        .recv()
        .await
        .expect("the action reached the controller");
    assert!(
        matches!(
            action.action,
            ControllerAction::SetConfig { ref key, ref value, .. }
                if key == "model" && value == "sonnet"
        ),
        "the advertised value was not forwarded unchanged"
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
}

/// A dirty-worktree acknowledgement names the repositories the person was
/// shown. A bare yes could be replayed against a set they never saw.
#[tokio::test]
async fn a_dirty_acknowledgement_is_bounded_and_names_repositories() {
    let oversized = (0..40)
        .map(|index| format!(r#""repo-{index}""#))
        .collect::<Vec<_>>()
        .join(",");
    for (ack, why) in [
        (oversized.as_str(), "an unbounded acknowledgement"),
        (r#""""#, "an empty repository name"),
    ] {
        let (app, mut actions, _, _, _) = app();
        let body = format!(
            r#"{{"action":"new","workspace_id":"default","profile_id":"codex-1","bundle_id":"hel","target_id":"podman","dirty_ack":[{ack}]}}"#
        );
        let response = post_action(app, cookie(), body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
        assert!(actions.try_recv().is_err(), "{why} reached the controller");
    }
}

/// A session created without a title still gets one, derived the way the
/// terminal derives it, so the two surfaces name a session alike.
#[tokio::test]
async fn a_new_session_without_a_title_is_accepted() {
    let (app, mut actions, _, _, _) = app();
    let response = tokio::spawn(post_action(
        app,
        cookie(),
        r#"{"action":"new","workspace_id":"default","profile_id":"codex-1","bundle_id":"hel","target_id":"podman"}"#
            .to_owned(),
    ));
    // The handler answers only once the controller does, so the reply has
    // to be sent before the response can be read.
    let action = actions
        .recv()
        .await
        .expect("the action reached the controller");
    assert!(
        matches!(
            action.action,
            ControllerAction::New { title: None, ref workspace_id, .. }
                if workspace_id == "default"
        ),
        "the workspace or the absent title did not survive the boundary"
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
}

/// Two phones must not share stored state, and one phone's state must
/// survive its own re-login. Neither is true of a cookie that signs only
/// an expiry, which is what this replaced.
#[test]
fn a_cookie_names_one_viewer_and_two_cookies_never_collide() {
    let key = b"01234567890123456789012345678901";
    let expiry = now_unix().saturating_add(3600);
    let first = signed_cookie_value(key, "viewer-a", expiry);
    let second = signed_cookie_value(key, "viewer-b", expiry);
    assert_ne!(
        first, second,
        "two viewers unlocking in the same second share a cookie"
    );
    assert_eq!(
        cookie_viewer(key, &first, now_unix()),
        Some("viewer-a".to_owned())
    );
    assert_eq!(
        cookie_viewer(key, &second, now_unix()),
        Some("viewer-b".to_owned())
    );
}

/// A forged or tampered cookie names nobody.
#[test]
fn a_tampered_cookie_is_refused() {
    let key = b"01234567890123456789012345678901";
    let expiry = now_unix().saturating_add(3600);
    let honest = signed_cookie_value(key, "viewer-a", expiry);
    let swapped = honest.replacen("viewer-a", "viewer-b", 1);
    assert_eq!(cookie_viewer(key, &swapped, now_unix()), None);
    assert_eq!(cookie_viewer(key, "nonsense", now_unix()), None);
    assert_eq!(cookie_viewer(key, &format!("{expiry}."), now_unix()), None);
}

/// A composer is for a prompt. The bound exists so one viewer cannot fill
/// the daemon's database with text it never sent.
#[tokio::test]
async fn an_oversized_draft_is_refused_with_a_stable_code() {
    let (app, _, _, _, mut stored) = app();
    let draft = "x".repeat(64 * 1024 + 1);
    let response = app
        .oneshot(
            Request::put("/api/sessions/session-1/draft")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "draft": draft }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(stored.try_recv().is_err(), "an oversized draft was stored");
}

/// A search that is not a search is refused before it reaches a database.
#[tokio::test]
async fn prompt_history_refuses_an_unknown_scope() {
    let (app, _, _, _, mut stored) = app();
    let response = app
        .oneshot(
            Request::get("/api/sessions/session-1/history?q=ship&scope=everything")
                .header(COOKIE, cookie())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        stored.try_recv().is_err(),
        "the search reached the controller"
    );
}

/// A preflight starts nothing. It answers the questions a person needs
/// before committing, and it refuses an impossible combination there
/// rather than after the commit.
/// Every preflight test in this module asks about a new session; a resume
/// preflight shares the channel but never these fixtures.
fn new_preflight(request: PreflightRequest) -> NewPreflightRequest {
    match request {
        PreflightRequest::New(request) => request,
        other => panic!("expected a new-session preflight, got {other:?}"),
    }
}

#[tokio::test]
async fn a_preflight_validates_before_it_reaches_the_controller() {
    for (body, why) in [
        (
            r#"{"profile_id":"nope","bundle_id":"hel","target_id":"podman"}"#,
            "an unknown profile",
        ),
        (
            r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"raw"}"#,
            "a bare target with no directory",
        ),
    ] {
        let (app, _, _, mut preflights, _) = app();
        let response = app
            .oneshot(
                Request::post("/api/preflight/new")
                    .header(COOKIE, cookie())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
        assert!(
            preflights.try_recv().is_err(),
            "{why} reached the controller"
        );
    }
}

/// A bare target opens a directory the person named. The controller still
/// validates that directory before answering, because the server's state
/// projection cannot inspect the filesystem or an SSH host.
#[tokio::test]
async fn a_bare_preflight_forwards_directory_validation_to_the_controller() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(app.oneshot(
            Request::post("/api/preflight/new")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"raw","project_directory":"~/project"}"#,
                ))
                .unwrap(),
        ));
    let request = new_preflight(preflights.recv().await.expect("the controller was asked"));
    assert_eq!(request.bundle_id, "hel");
    assert_eq!(request.target_id, "raw");
    assert_eq!(request.project_directory, Some(PathBuf::from("~/project")));
    request
        .reply
        .send(Ok(PreflightNew {
            managed_worktree: Default::default(),
            project_directory: Some("/remote/project".into()),
            remote_repairs: Vec::new(),
            dirty_repositories: Vec::new(),
            remote_repositories: Vec::new(),
            local_changes_excluded: false,
        }))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let answer: PreflightNew = serde_json::from_slice(&body).unwrap();
    assert!(answer.dirty_repositories.is_empty());
    assert_eq!(answer.project_directory, Some("/remote/project".into()));
}

/// The resume card cannot warn about a checkout it has not asked about.
/// The route has to reach the controller and hand the answer back whole.
#[tokio::test]
async fn a_resume_preflight_returns_the_conversion_preview_it_was_given() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/preflight/resume")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"session_id":"session-1","target_id":"podman"}"#,
                ))
                .unwrap(),
        ),
    );
    let request = preflights.recv().await.expect("the controller was asked");
    let PreflightRequest::Resume(request) = request else {
        panic!("expected a resume preflight");
    };
    assert_eq!(request.session_id, "session-1");
    assert_eq!(request.target_id, "podman");
    request
        .reply
        .send(Ok(PreflightResume::ConvertingRawCheckout {
            preview: Box::new(mj_core::state::RawConversionPreview {
                checkout: "/work/repo".into(),
                destination: "/workspace/repo".into(),
                branch: Some("mj/session-1".into()),
                fetch_url: "https://github.com/example/repo.git".into(),
                push_urls: Vec::new(),
                default_branch: "main".into(),
                unpushed_commits: 1,
                staged_files: 0,
                unstaged_files: 1,
                untracked_files: 0,
                untracked_bytes: 0,
                host_checkout_retained: true,
            }),
        }))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer["kind"], "converting-raw-checkout");
    assert_eq!(answer["preview"]["branch"], "mj/session-1");
    assert_eq!(answer["preview"]["host_checkout_retained"], true);
}

/// A session the projection does not have never reaches the controller.
#[tokio::test]
async fn a_resume_preflight_for_an_unknown_session_is_refused_without_the_controller() {
    let (app, _, _, mut preflights, _) = app();
    let response = app
        .oneshot(
            Request::post("/api/preflight/resume")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"session_id":"missing","target_id":"podman"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(preflights.try_recv().is_err());
}

#[tokio::test]
async fn a_bare_preflight_validation_failure_is_actionable_without_its_details() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(app.oneshot(
        Request::post("/api/preflight/new")
            .header(COOKIE, cookie())
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"raw","project_directory":"/private/project"}"#,
            ))
            .unwrap(),
    ));
    let request = new_preflight(preflights.recv().await.expect("the controller was asked"));
    request
        .reply
        .send(Err(PreflightFailure::Validation))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "error": "project validation failed; check that the directory exists, is accessible, and contains a Git repository with a valid HEAD"
        })
    );
    assert!(!String::from_utf8_lossy(&body).contains("/private/project"));
}

#[tokio::test]
async fn a_bundle_preflight_controller_failure_keeps_the_generic_service_error() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/preflight/new")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"podman"}"#,
                ))
                .unwrap(),
        ),
    );
    let request = new_preflight(preflights.recv().await.expect("the controller was asked"));
    request
        .reply
        .send(Err(PreflightFailure::Controller(
            "private /source/hel details".into(),
        )))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "the controller could not check this project"})
    );
    assert!(!String::from_utf8_lossy(&body).contains("/source/hel"));
}

/// An isolated bundle preflight returns the network clone plan so the
/// person can review it before creation.
#[tokio::test]
async fn a_bundle_preflight_reports_network_sources_and_excludes_local_changes() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/preflight/new")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"podman"}"#,
                ))
                .unwrap(),
        ),
    );
    let request = new_preflight(preflights.recv().await.expect("the controller was asked"));
    assert_eq!(request.bundle_id, "hel");
    assert_eq!(request.target_id, "podman");
    assert_eq!(request.project_directory, None);
    request
        .reply
        .send(Ok(PreflightNew {
            managed_worktree: Default::default(),
            project_directory: None,
            remote_repairs: Vec::new(),
            dirty_repositories: Vec::new(),
            remote_repositories: vec![PreflightRepository {
                id: "hel".into(),
                fetch_url: "https://github.com/example/hel.git".into(),
                default_branch: "main".into(),
                push_urls: vec!["ssh://git@example/hel.git".into()],
            }],
            local_changes_excluded: true,
        }))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let answer: PreflightNew = serde_json::from_slice(&body).unwrap();
    assert!(answer.dirty_repositories.is_empty());
    assert!(answer.local_changes_excluded);
    assert_eq!(answer.remote_repositories[0].default_branch, "main");
    assert_eq!(answer.remote_repositories[0].push_urls.len(), 1);
}

/// The browser names a target and a kind; the controller has to be asked
/// about that machine, not the controller's own disk, and the candidates
/// have to reach the browser unchanged.
#[tokio::test]
async fn a_path_completion_is_forwarded_with_its_host_and_kind() {
    let (app, _, _, mut preflights, _) = app();
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/paths/complete")
                .header(COOKIE, cookie())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target_id":"raw","prefix":"/srv/pr","kind":"any"}"#,
                ))
                .unwrap(),
        ),
    );
    let request = preflights.recv().await.expect("the controller was asked");
    let PreflightRequest::CompletePath(request) = request else {
        panic!("expected a path completion");
    };
    assert_eq!(request.host, CompletionHost::Target("raw".into()));
    assert_eq!(request.prefix, "/srv/pr");
    assert_eq!(request.kind, CompletionKind::Any);
    request
        .reply
        .send(Ok(PathCompletion {
            candidates: vec!["/srv/projects/".into(), "/srv/prompts.txt".into()],
            insert: Some("/srv/pro".into()),
            truncated: false,
        }))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "candidates": ["/srv/projects/", "/srv/prompts.txt"],
            "insert": "/srv/pro",
            "truncated": false,
        })
    );
}

/// Neither an unknown target nor an implausible prefix is worth a shell.
#[tokio::test]
async fn a_path_completion_rejects_unknown_targets_and_oversized_prefixes() {
    let long_prefix = "/".repeat(4097);
    for (body, why) in [
        (
            r#"{"target_id":"missing","prefix":"/srv/"}"#.to_owned(),
            "an unknown target",
        ),
        (
            serde_json::json!({ "prefix": long_prefix }).to_string(),
            "an oversized prefix",
        ),
    ] {
        let (app, _, _, mut preflights, _) = app();
        let response = app
            .oneshot(
                Request::post("/api/paths/complete")
                    .header(COOKIE, cookie())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
        assert!(
            preflights.try_recv().is_err(),
            "{why} reached the controller"
        );
    }
}

/// Completion lists directories on the controller's machines, so it lives
/// behind the viewer cookie like every other controller question.
#[tokio::test]
async fn a_path_completion_requires_a_session() {
    let (app, _, _, mut preflights, _) = app();
    let response = app
        .oneshot(
            Request::post("/api/paths/complete")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"prefix":"/srv/"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(preflights.try_recv().is_err());
}

/// Everything an agent writes goes through the Markdown renderer, so the
/// renderer is where injection is stopped. These checks run the shipped
/// module against a fake DOM: structure has to come out as elements, and
/// markup an agent typed has to come out as text.
#[test]
fn the_markdown_renderer_builds_structure_and_refuses_injection() {
    run_web_check(
        "markdown",
        r#"import { installDocument, elements, only, check, checkEqual } from './test-dom.js';
installDocument();
const { renderMarkdown, renderDiffSummary, safeHref } = await import('./markdown.js');

const render = source => {
  const host = document.createElement('section');
  host.append(renderMarkdown(source));
  return host;
};

// Headings
checkEqual(only(render('# Title'), 'h1').textContent, 'Title', 'h1');
checkEqual(only(render('### Deep'), 'h3').textContent, 'Deep', 'h3');

// Nested lists
const nested = render('- one\n  - inner\n- two');
check(elements(nested, 'ul').length === 2, 'nested list produced ' + elements(nested, 'ul').length + ' lists');
check(elements(elements(nested, 'ul')[0], 'li').length >= 2, 'outer list lost items');

// Ordered lists
checkEqual(elements(render('1. a\n2. b'), 'ol').length, 1, 'ordered list');

// Fenced code stays unparsed
const fenced = render('```rust\nlet x = *y*;\n```');
checkEqual(only(fenced, 'code').textContent, 'let x = *y*;', 'fenced code');
check(elements(fenced, 'span').some(s => s.className === 'tok-kw'), 'fenced rust untinted');
checkEqual(elements(fenced, 'em').length, 0, 'fence emphasised its contents');
checkEqual(only(fenced, 'pre').dataset.lang, 'rust', 'fence language');

// Inline code beats emphasis
checkEqual(only(render('`*not em*`'), 'code').textContent, '*not em*', 'inline code');
checkEqual(elements(render('`*not em*`'), 'em').length, 0, 'inline code emphasised');

// Emphasis
checkEqual(only(render('**bold**'), 'strong').textContent, 'bold', 'strong');
checkEqual(only(render('*it*'), 'em').textContent, 'it', 'em');
checkEqual(only(render('~~gone~~'), 'del').textContent, 'gone', 'del');

// Tables
const table = render('| a | b |\n| --- | ---: |\n| 1 | 2 |');
checkEqual(elements(table, 'table').length, 1, 'table');
checkEqual(elements(table, 'th').length, 2, 'table header cells');
checkEqual(elements(table, 'td').length, 2, 'table body cells');
checkEqual(elements(table, 'th')[1].className, 'align-right', 'table alignment class');
checkEqual(only(table, 'div').className, 'scroll-x', 'table scroll wrapper');

// Blockquote and rule
checkEqual(elements(render('> quoted'), 'blockquote').length, 1, 'blockquote');
checkEqual(elements(render('---'), 'hr').length, 1, 'rule');

// XSS: markup is text, never elements
const injected = render('<img src=x onerror=alert(1)>');
checkEqual(elements(injected, 'img').length, 0, 'raw HTML became an element');
check(injected.textContent.includes('<img src=x onerror=alert(1)>'), 'raw HTML lost its text');

// XSS: refused link schemes
for (const target of ['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'java\tscript:alert(1)', 'data:text/html,<script>', 'vbscript:x']) {
  const out = render(`[click](${target})`);
  checkEqual(elements(out, 'a').length, 0, `link scheme ${JSON.stringify(target)} was allowed`);
  check(out.textContent.includes('click'), `link scheme ${JSON.stringify(target)} lost its label`);
}

// Accepted schemes keep their href and carry safe rel/target
for (const target of ['https://example.com', 'http://example.com/a', 'mailto:someone@example.com']) {
  const anchor = only(render(`[click](${target})`), 'a');
  checkEqual(anchor.getAttribute('href'), target, 'href');
  checkEqual(anchor.getAttribute('rel'), 'noreferrer noopener', 'rel');
  checkEqual(anchor.getAttribute('target'), '_blank', 'target');
}

// safeHref directly
checkEqual(safeHref('javascript:alert(1)'), null, 'safeHref allowed javascript:');
checkEqual(safeHref(' https://x.test '), 'https://x.test', 'safeHref cleaned value');

// Inline markup inside a link label
checkEqual(only(render('[**bold link**](https://x.test)'), 'strong').textContent, 'bold link', 'link label markup');

// An unclosed delimiter is literal, not markup
checkEqual(render('a * b').textContent, 'a * b', 'unclosed emphasis');
checkEqual(elements(render('a * b'), 'em').length, 0, 'unclosed emphasis made an element');

// Diff summaries: the real format from format_diffstat, two spaces and U+2212
const diff = renderDiffSummary(['src/main.rs  +12 −3', 'unparseable line']);
const items = elements(diff, 'li');
checkEqual(items.length, 2, 'diffstat rows');
checkEqual(elements(items[0], 'span')[0].textContent, 'src/main.rs', 'diffstat path');
checkEqual(elements(items[0], 'span')[1].textContent, '+12', 'diffstat additions');
checkEqual(elements(items[0], 'span')[2].textContent, '−3', 'diffstat deletions');
checkEqual(elements(items[1], 'span').length, 1, 'unparseable diffstat produced counts');
checkEqual(elements(items[1], 'span')[0].textContent, 'unparseable line', 'unparseable diffstat lost its text');

console.log('all markdown checks passed');
"#,
    );
}

/// Tool output is not prose, and rendering it as prose loses the parts
/// that matter: which words in a command are the program and which are
/// paths, where a JSON payload begins, and whether a five-thousand-line
/// dump has to be paid for before anyone asks to see it.
#[test]
fn tool_output_is_tinted_folded_and_never_read_as_markdown() {
    run_web_check(
        "tool-output",
        r#"import { installDocument, elements, only, check, checkEqual, openFold } from './test-dom.js';
installDocument();
const { renderToolOutput, codeBlock, detectLang, appendCommandTokens, isPathLike } = await import(
  './tool-output.js'
);

const classes = root => elements(root, 'span').map(s => s.className);

// A shell command is told apart into program, subcommand, flag and path.
const line = document.createElement('pre');
appendCommandTokens(line, 'cargo test --workspace src/lib.rs');
const seen = classes(line);
check(seen.includes('cmd-program'), 'no program: ' + seen);
check(seen.includes('cmd-subcommand'), 'no subcommand: ' + seen);
check(seen.includes('cmd-flag'), 'no flag: ' + seen);
check(seen.includes('cmd-path'), 'no path: ' + seen);
checkEqual(line.textContent, 'cargo test --workspace src/lib.rs', 'command text changed');

// An operator starts the program count again, so both programs are found.
const piped = document.createElement('pre');
appendCommandTokens(piped, 'git status && cargo build');
checkEqual(classes(piped).filter(c => c === 'cmd-program').length, 2, 'pipeline reset');

// Prose with a slash is not a path; a real path is.
check(!isPathLike('and/or'), '"and/or" read as a path');
check(isPathLike('src/lib/thing.rs'), 'a real path did not');
check(isPathLike('./x'), 'a relative path did not');
check(isPathLike('Cargo.toml'), 'a file with an extension did not');

// JSON is pretty-printed and tinted, keys apart from values.
const json = renderToolOutput('{"name":"hel","count":3,"ok":true}');
const jsonClasses = classes(json);
check(jsonClasses.includes('tok-key'), 'no JSON key: ' + jsonClasses);
check(jsonClasses.includes('tok-str'), 'no JSON string: ' + jsonClasses);
check(jsonClasses.includes('tok-num'), 'no JSON number: ' + jsonClasses);
check(jsonClasses.includes('tok-kw'), 'no JSON keyword: ' + jsonClasses);
check(json.textContent.includes('"name"'), 'JSON lost its content');

// Rust is tinted; an unknown language is not.
const rust = codeBlock('pub fn main() {\n    let x = 1;\n}', 'rust');
check(classes(rust).includes('tok-kw'), 'rust keywords untinted');
checkEqual(only(rust, 'pre').dataset.lang, 'rust', 'rust data-lang');
const plain = codeBlock('nothing in particular here', 'brainfuck');
checkEqual(classes(plain).length, 0, 'unknown language was tinted');

// Sniffing is conservative: a log stays plain, real code does not.
checkEqual(detectLang('12:03 INFO started\n12:04 INFO done\n12:05 INFO stopped'), '', 'a log was sniffed');
checkEqual(
  detectLang('fn a() {}\nfn b() {}\nlet mut x = 1;\nuse std::fmt;\nimpl Foo {}\nlet y = x.unwrap();'),
  'rust',
  'rust was not sniffed',
);
checkEqual(detectLang('--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new'), 'diff', 'diff was not sniffed');

// A long dump is one closed fold that has built nothing yet.
const long = Array.from({ length: 400 }, (_, i) => `line ${i}`).join('\n');
const folded = renderToolOutput(long);
checkEqual(folded.nodeName, 'DETAILS', 'a 400-line dump was not folded');
checkEqual(elements(folded, 'pre').length, 0, 'a closed fold built its content anyway');
check(only(folded, 'summary').textContent.includes('400 lines'), 'fold summary: ' + only(folded, 'summary').textContent);
openFold(folded);
checkEqual(elements(folded, 'pre').length, 1, 'an opened fold built nothing');
check(elements(folded, 'pre')[0].textContent.includes('line 399'), 'the fold lost its content');

// Opening twice builds once.
openFold(folded);
checkEqual(elements(folded, 'pre').length, 1, 'reopening rebuilt the content');

// A short dump is not folded.
checkEqual(renderToolOutput('one\ntwo').nodeName, 'PRE', 'a short dump was folded');

// Tool output is never parsed as Markdown, so an underscore is an underscore.
const literal = renderToolOutput('a _b_ c <img src=x>');
checkEqual(elements(literal, 'em').length, 0, 'tool output was emphasised');
checkEqual(elements(literal, 'img').length, 0, 'tool output produced an element');
check(literal.textContent.includes('<img src=x>'), 'tool output lost its text');

console.log('all tool-output checks passed');
"#,
    );
}

/// The renderer's guarantee is structural — this code cannot inject markup
/// because it never builds markup — and a single stray assignment would
/// quietly replace it with no guarantee at all. `escapeHtml` uses
/// `innerHTML` on a detached node to escape text, which is safe but is
/// also exactly the shape this test exists to stop spreading, so it is
/// named rather than pattern-matched.
#[test]
fn no_web_module_builds_markup_from_a_string() {
    const SINKS: [&str; 5] = [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "new Function",
    ];
    // There is no allowance. Every one of these sinks was removed in
    // Milestone 2, and the point of the test is that none comes back.
    const ALLOWED: [(&str, &str); 0] = [];
    for (name, source) in [
        ("viewer.js", VIEWER_JS),
        ("markdown.js", MARKDOWN_JS),
        ("tool-output.js", TOOL_OUTPUT_JS),
    ] {
        for (number, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            for sink in SINKS {
                if !trimmed.contains(sink) {
                    continue;
                }
                assert!(
                    ALLOWED
                        .iter()
                        .any(|(file, allowed)| *file == name && trimmed == *allowed),
                    "{name}:{} builds markup from a string: {trimmed}",
                    number + 1
                );
            }
        }
    }
}

/// The card cache is the fix for answers vanishing under snapshot polls, so
/// it is exercised as JavaScript: the render source is lifted out of
/// `src/web/viewer.js` and run against a stub DOM.
#[test]
fn embedded_viewer_keeps_elicitation_answers_across_snapshot_polls() {
    let source = viewer_source(
        "const elicitationCards = new Map()",
        "async function submitElicitation",
    );
    let dom = r#"
let replaceCalls = 0;
function makeEl(tag) {
  return {
tagName: tag.toUpperCase(),
children: [],
options: [],
selectedOptions: [],
className: "",
textContent: "",
disabled: false,
required: false,
value: "",
appendChild(child) {
  this.children.push(child);
  if (this.tagName === "SELECT") this.options.push(child);
  return child;
},
append(...kids) {
  this.children.push(...kids);
},
replaceChildren(...kids) {
  replaceCalls += 1;
  this.children = kids;
},
addEventListener() {},
querySelectorAll(selector) {
  const found = [];
  const visit = node => {
    for (const child of node.children) {
      if (child.tagName === "INPUT" && (selector === "input" || child.checked)) found.push(child);
      visit(child);
    }
  };
  visit(this);
  return found;
},
querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
setCustomValidity() {},
reportValidity() {
  return true;
},
  };
}
const created = [];
const document = {
  createElement(tag) {
const el = makeEl(tag);
created.push(el);
return el;
  },
};
const elicitations = makeEl("div");
function el(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className || "";
  node.textContent = text || "";
  return node;
}
async function submitElicitation() {}
"#;
    let checks = r#"
const request = {
  id: "elicitation-1",
  message: "Which CI architecture?",
  title: "CI",
  fields: [
{
  id: "question_0",
  title: "CI architecture",
  required: false,
  kind: "single_select",
  options: [{ value: "reusable", title: "Reusable" }, { value: "matrix", title: "Matrix" }],
},
{ id: "question_0_custom", title: "Other", required: false, kind: "text" },
  ],
};
const session = { id: "session-1", pending_elicitations: [request] };
renderElicitations(session);
const card = elicitations.children[0];
const radio = created.find((el) => el.tagName === "INPUT" && el.value === "reusable");
const text = created.find((el) => el.tagName === "INPUT" && el.type === "text");
radio.checked = true;
text.value = "keep me";
const attachments = replaceCalls;
renderElicitations(session);
if (elicitations.children[0] !== card) {
  throw new Error("a snapshot rebuilt the pending card");
}
if (!radio.checked || text.value !== "keep me") {
  throw new Error("a snapshot wiped the half-filled answer");
}
if (replaceCalls !== attachments) {
  throw new Error("a snapshot re-attached an unchanged card and dropped focus");
}
sentElicitations.add(elicitationKey("session-1", request.id));
renderElicitations(session);
if (elicitations.children[0] !== card) {
  throw new Error("a sent answer rebuilt the card");
}
if (!radio.disabled || !text.disabled) {
  throw new Error("a sent answer left the controls live");
}
if (!radio.checked) {
  throw new Error("a sent answer wiped the reply");
}
renderElicitations({ id: "session-1", pending_elicitations: [] });
if (elicitations.children.length !== 0 || elicitationCards.size !== 0) {
  throw new Error("an answered request stayed rendered");
}
if (sentElicitations.size !== 0) {
  throw new Error("a resolved request kept its sent marker");
}
"#;
    run_viewer_script(
        "elicitation-rendering",
        &format!("{dom}\n{source}\n{checks}"),
    );
}

fn sample_image(pixels: usize) -> ViewerPromptImage {
    ViewerPromptImage {
        data_base64: base64::engine::general_purpose::STANDARD.encode(vec![7_u8; pixels]),
        mime_type: "image/png".into(),
        width: 32,
        height: 24,
        attachment: None,
    }
}

fn sample_valid_image() -> ViewerPromptImage {
    ViewerPromptImage {
        data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
            .into(),
        mime_type: "image/png".into(),
        width: 1,
        height: 1,
        attachment: None,
    }
}

fn image_capable(snapshot: &mut ViewerSnapshot) {
    snapshot.sessions[0].prompt_images_supported = true;
}

async fn post_action(app: Router, cookie: String, body: String) -> Response<Body> {
    app.oneshot(
        Request::post("/api/actions")
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn image_prompt_reaches_the_controller_with_its_images() {
    let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
    let cookie = login_cookie(&app).await;
    let image = sample_valid_image();
    let body = serde_json::to_string(&ControllerAction::Prompt {
        command_id: None,
        session_id: "session-1".into(),
        text: String::new(),
        images: vec![image.clone(), image.clone()],
    })
    .unwrap();
    let response = tokio::spawn(post_action(app, cookie, body));
    let request = actions.recv().await.unwrap();
    let ControllerRequest { action, reply } = request;
    let ControllerAction::Prompt {
        session_id,
        text,
        images,
        ..
    } = action
    else {
        panic!("expected a prompt action")
    };
    assert_eq!(session_id, "session-1");
    assert!(text.is_empty());
    assert_eq!(images.len(), 2);
    assert!(
        images
            .iter()
            .all(|image| { image.data_base64.is_empty() && image.attachment.is_some() })
    );
    reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn browser_attachment_upload_returns_a_stored_reference_without_inline_bytes() {
    let (app, _, _, _, _) = app_with_snapshot(image_capable);
    let cookie = login_cookie(&app).await;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let response = app
        .oneshot(
            Request::post("/api/sessions/session-1/attachments")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "image/png")
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let image: ViewerPromptImage = serde_json::from_slice(&body).unwrap();
    assert!(image.data_base64.is_empty());
    let reference = image.attachment.expect("upload should return a reference");
    assert_eq!(reference.mime_type, "image/png");
    assert_eq!(reference.width, 1);
    assert_eq!(reference.height, 1);
    assert!(reference.size <= 700 * 1024);
}

/// Base64 inflates an upload by a third, so two ordinary photographs pass
/// the general body limit even when each one fits it. The action route
/// carries prompts, so it is the route that gets the larger bound.
#[tokio::test]
async fn multi_image_prompts_are_accepted_over_the_general_body_limit() {
    let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
    let cookie = login_cookie(&app).await;
    let image = sample_valid_image();
    let mut body = serde_json::to_string(&ControllerAction::Prompt {
        command_id: None,
        session_id: "session-1".into(),
        text: "look at these".into(),
        images: vec![image.clone(), image],
    })
    .unwrap();
    body.push_str(&" ".repeat(MAX_BODY_BYTES));
    assert!(body.len() > MAX_BODY_BYTES);
    assert!(body.len() < MAX_PROMPT_BODY_BYTES);
    let response = tokio::spawn(post_action(app, cookie, body));
    let action = actions.recv().await.unwrap();
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn a_body_over_the_prompt_limit_is_still_refused() {
    let (app, _actions, _, _, _) = app_with_snapshot(image_capable);
    let cookie = login_cookie(&app).await;
    let image = sample_image(MAX_PROMPT_BODY_BYTES);
    let body = serde_json::to_string(&ControllerAction::Prompt {
        command_id: None,
        session_id: "session-1".into(),
        text: String::new(),
        images: vec![image],
    })
    .unwrap();
    assert!(body.len() > MAX_PROMPT_BODY_BYTES);
    let response = post_action(app, cookie, body).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn malformed_image_payloads_never_reach_the_controller() {
    let cases = [
        ("aW1hZ2U=", "text/plain", 32, 24),
        ("aW1hZ2U=", "image/png", 0, 24),
        ("not base64!", "image/png", 32, 24),
        ("", "image/png", 32, 24),
    ];
    for (data, mime, width, height) in cases {
        let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
        let cookie = login_cookie(&app).await;
        let body = serde_json::to_string(&ControllerAction::Prompt {
            command_id: None,
            session_id: "session-1".into(),
            text: String::new(),
            images: vec![ViewerPromptImage {
                data_base64: data.into(),
                mime_type: mime.into(),
                width,
                height,
                attachment: None,
            }],
        })
        .unwrap();
        let response = post_action(app, cookie, body).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "expected {data:?}/{mime} {width}x{height} to be refused"
        );
        assert!(actions.try_recv().is_err());
    }
}

#[test]
fn image_prompts_need_text_or_an_image_and_an_agent_that_takes_them() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let prompt = |text: &str, images: Vec<ViewerPromptImage>| ControllerAction::Prompt {
        command_id: None,
        session_id: "session-1".into(),
        text: text.into(),
        images,
    };

    // Without the capability the session takes text only.
    assert!(validate_action(&prompt("ship it", Vec::new()), &snapshot).is_ok());
    assert!(validate_action(&prompt("", vec![sample_image(8)]), &snapshot).is_err());

    image_capable(&mut snapshot);
    // An image is a prompt on its own; nothing at all is not.
    assert!(validate_action(&prompt("", vec![sample_image(8)]), &snapshot).is_ok());
    assert!(
        validate_action(
            &prompt("", vec![sample_image(8); MAX_PROMPT_IMAGES + 1]),
            &snapshot,
        )
        .is_err()
    );
    assert!(validate_action(&prompt("   ", Vec::new()), &snapshot).is_err());
    assert!(validate_action(&prompt("", Vec::new()), &snapshot).is_err());
    // A shell command is still a shell command.
    assert!(validate_action(&prompt("!ls", vec![sample_image(8)]), &snapshot).is_err());
}

/// The composer holds a DOM, not a string, so the text a prompt sends is
/// whatever this reader makes of that DOM. Run it as JavaScript.
#[test]
fn embedded_viewer_reads_multiline_composer_text_out_of_its_dom() {
    let source = viewer_source("function composerText()", "function setComposerText(");
    let harness = r##"
const Node = { TEXT_NODE: 3 };
function textNode(value) {
  return { nodeType: 3, nodeValue: value, nodeName: "#text", childNodes: [], dataset: {} };
}
function element(name, children = [], dataset = {}) {
  const node = { nodeType: 1, nodeName: name, dataset, childNodes: children };
  children.forEach((child, index) => {
child.nextSibling = children[index + 1] || null;
  });
  return node;
}
let promptText = null;
function read(children) {
  promptText = element("DIV", children);
  return composerText();
}
"##;
    let checks = r#"
const plain = read([textNode("ship it")]);
if (plain !== "ship it") throw new Error(`plain text became ${JSON.stringify(plain)}`);

const broken = read([textNode("first"), element("BR"), textNode("second")]);
if (broken !== "first\nsecond") throw new Error(`line break became ${JSON.stringify(broken)}`);

// The trailing break a browser leaves behind to keep the caret on a new line
// is scaffolding, not a line the user typed.
const filler = read([
  textNode("first"),
  element("BR"),
  element("BR", [], { composerFiller: "true" }),
]);
if (filler !== "first\n") throw new Error(`filler break became ${JSON.stringify(filler)}`);

const blocks = read([
  textNode("first"),
  element("DIV", [textNode("second")]),
  element("DIV", [textNode("third")]),
]);
if (blocks !== "first\nsecond\nthird") throw new Error(`blocks became ${JSON.stringify(blocks)}`);

const carriage = read([textNode("first\r\nsecond")]);
if (carriage !== "first\nsecond") throw new Error(`CRLF became ${JSON.stringify(carriage)}`);
"#;
    run_viewer_script("composer-reader", &format!("{harness}\n{source}\n{checks}"));
}

/// A page that declares no icon makes every browser request
/// `/favicon.ico`, which this server does not have. The page therefore has
/// to name an icon, and that icon has to be served.
#[tokio::test]
async fn viewer_declares_the_icon_route_instead_of_requesting_a_missing_favicon() {
    let (app, _, _, _, _) = app();
    let page = fetch_text(app.clone(), "/").await;
    assert!(page.contains(r#"rel="icon""#), "the page declares no icon");
    assert!(page.contains("/icon.svg"), "the page names no icon route");
    let icon = app
        .oneshot(Request::get("/icon.svg").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(icon.status(), StatusCode::OK);
    assert_eq!(
        icon.headers().get(CONTENT_TYPE).unwrap(),
        "image/svg+xml",
        "the icon route does not serve an SVG"
    );
}

#[tokio::test]
async fn valid_action_is_typed_and_forwarded() {
    let (app, mut actions, _, _, _) = app();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"prompt","session_id":"session-1","text":"ship it"}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert_eq!(
        action.action,
        ControllerAction::Prompt {
            command_id: None,
            session_id: "session-1".into(),
            text: "ship it".into(),
            images: Vec::new(),
        }
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn move_preparation_is_read_only_and_returns_the_daemon_fingerprint() {
    let (app, mut preparations) = app_with_move_receiver();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/moves/prepare")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"session_id":"session-1","profile_id":"codex-1","target_template_id":"podman","clear_resource_allocation":false,"additional_mounts":null,"resource_allocation":null}"#,
                ))
                .unwrap(),
        ),
    );
    let request = preparations
        .recv()
        .await
        .expect("preparation reached daemon");
    assert_eq!(request.selection.session_id, "session-1");
    assert_eq!(request.selection.profile_id.as_deref(), Some("codex-1"));
    assert_eq!(
        request.selection.target_template_id.as_deref(),
        Some("podman")
    );
    request
        .reply
        .send(Ok(MovePreparation {
            in_place: false,
            source_unavailable: false,
            conversion: None,
            selection: request.selection,
            source_profile_id: "codex-1".into(),
            source_target_template_id: "podman".into(),
            cross_harness: false,
            active: true,
            queued_commands: vec![mj_core::state::MaterializedQueuedPrompt {
                accepted_ordinal: None,
                command_id: "queued-1".into(),
                kind: mj_core::state::QueuedCommandKind::Prompt,
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "[Image attachment: image/png]"
                })],
                queued_at_ms: 1,
            }],
            fingerprint: "fingerprint".into(),
            operation_id: "move-1".into(),
        }))
        .unwrap();
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["operation_id"], "move-1");
    assert_eq!(body["active"], true);
    assert_eq!(
        body["queued_commands"][0]["content"][0]["text"],
        "[Image attachment: image/png]"
    );
}

#[tokio::test]
async fn confirmed_move_action_forwards_the_fingerprinted_request() {
    let (app, mut actions, _, _, _) = app_with_snapshot(|snapshot| {
        snapshot.sessions[0].capabilities.move_session = true;
    });
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"move","request":{"preparation":{"selection":{"session_id":"session-1","profile_id":"codex-1","target_template_id":"podman","clear_resource_allocation":false,"additional_mounts":null,"resource_allocation":null},"source_profile_id":"codex-1","source_target_template_id":"podman","cross_harness":false,"active":false,"queued_commands":[],"fingerprint":"fingerprint","operation_id":"move-1"},"queue":null,"acknowledge_interruption":false}}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.expect("move action reached daemon");
    assert!(matches!(action.action, ControllerAction::Move { .. }));
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn shell_action_is_typed_and_forwarded() {
    let (app, mut actions, _, _, _) = app();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"run-shell","session_id":"session-1","command":"cargo test"}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert_eq!(
        action.action,
        ControllerAction::RunShell {
            command_id: None,
            session_id: "session-1".into(),
            command: "cargo test".into(),
        }
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[test]
fn shell_action_validation_reserves_bang_prompts_and_checks_cancellation_ids() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    assert!(
        validate_action(
            &ControllerAction::Prompt {
                command_id: None,
                session_id: "session-1".into(),
                text: "!cargo test".into(),
                images: Vec::new(),
            },
            &snapshot,
        )
        .is_err()
    );
    assert!(
        validate_action(
            &ControllerAction::RunShell {
                command_id: None,
                session_id: "session-1".into(),
                command: "cargo test".into(),
            },
            &snapshot,
        )
        .is_ok()
    );
    assert!(
        validate_action(
            &ControllerAction::CancelShell {
                session_id: "session-1".into(),
                shell_command_id: "shell-1".into(),
            },
            &snapshot,
        )
        .is_err()
    );

    snapshot.sessions[0]
        .active_user_shells
        .push(ViewerUserShell {
            id: "shell-1".into(),
            command: "cargo test".into(),
            started_at_ms: Some(10),
        });
    assert!(
        validate_action(
            &ControllerAction::CancelShell {
                session_id: "session-1".into(),
                shell_command_id: "shell-1".into(),
            },
            &snapshot,
        )
        .is_ok()
    );
}

#[tokio::test]
async fn bare_new_action_forwards_an_explicit_safe_project_directory() {
    let (app, mut actions, _, _, _) = app();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"new","profile_id":"codex-1","bundle_id":"hel","target_id":"raw","title":"Raw work","project_directory":"/work/project"}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert_eq!(
        action.action,
        ControllerAction::New {
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: String::new(),
            profile_id: "codex-1".into(),
            bundle_id: "hel".into(),
            target_id: "raw".into(),
            title: Some("Raw work".into()),
            project_directory: Some(PathBuf::from("/work/project")),
            dirty_ack: Vec::new(),
        }
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[test]
fn new_action_requires_project_directory_exactly_for_bare_targets() {
    let (config, state) = sample_config_state();
    let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    let action = |target_id: &str, project_directory: Option<PathBuf>| ControllerAction::New {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: String::new(),
        profile_id: "codex-1".into(),
        bundle_id: "hel".into(),
        target_id: target_id.into(),
        title: Some("New work".into()),
        project_directory,
        dirty_ack: Vec::new(),
    };

    assert!(validate_action(&action("podman", None), &snapshot).is_ok());
    assert_eq!(
        validate_action(&action("podman", Some("/work".into())), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        validate_action(&action("raw", None), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        validate_action(&action("raw", Some("relative".into())), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        validate_action(&action("raw", Some("/work/../secret".into())), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    assert!(validate_action(&action("raw", Some("/work/project".into())), &snapshot).is_ok());
}

#[tokio::test]
async fn cancel_action_is_typed_and_forwarded() {
    let (app, mut actions, _, _, _) = app();
    let cookie = login_cookie(&app).await;
    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"cancel","session_id":"session-1"}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();
    assert_eq!(
        action.action,
        ControllerAction::Cancel {
            session_id: "session-1".into(),
        }
    );
    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn action_validation_accepts_cross_harness_resume_and_rejects_unknown() {
    let (mut config, state) = sample_config_state();
    config.profiles.insert(
        "claude-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Claude,
            home: "/secret/claude".into(),
            environment: BTreeMap::new(),
        },
    );
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    snapshot.workspaces.push(ViewerWorkspace {
        id: "workspace-1".into(),
        name: "One".into(),
    });
    validate_action(
        &ControllerAction::Resume {
            session_id: "session-1".into(),
            workspace_id: "workspace-1".into(),
            profile_id: "claude-1".into(),
            target_id: "podman".into(),
            queue: ResumeQueueDisposition::Start,
            additional_mounts: None,
            resource_allocation: None,
        },
        &snapshot,
    )
    .unwrap();

    let error = validate_action(
        &ControllerAction::Resume {
            session_id: "session-1".into(),
            workspace_id: "missing".into(),
            profile_id: "claude-1".into(),
            target_id: "podman".into(),
            queue: ResumeQueueDisposition::Start,
            additional_mounts: None,
            resource_allocation: None,
        },
        &snapshot,
    )
    .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);

    let error = validate_action(
        &ControllerAction::Suspend {
            session_id: "not-managed".into(),
        },
        &snapshot,
    )
    .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
}

/// A review the daemon is running reaches the phone whole: its tier, what
/// each reviewing agent is doing, and the findings to answer.
#[test]
fn a_running_review_projects_to_the_phone() {
    use crate::review_host::{RuntimeReviewView, VerdictKind, VerdictView};
    use mj_core::review::driver::{Resolution, RoleState, RoleStatus, TurnReviewPhase};

    let review = RuntimeReviewView {
        session_id: "session-1".into(),
        tier: mj_core::review::lanes::ReviewTier::Extended,
        phase: TurnReviewPhase::Verdict(mj_core::review::verdict::ReviewVerdict::Findings {
            synthesis: "[P1] src/lib.rs:1 -- unbounded retry".into(),
            evidence: Default::default(),
        }),
        roles: vec![
            RoleStatus {
                role: "supervisor".into(),
                label: "Supervisor".into(),
                state: RoleState::Clean,
            },
            RoleStatus {
                role: "tests".into(),
                label: "Tests".into(),
                state: RoleState::Findings,
            },
        ],
        status: "Enter to act".into(),
        verdict: Some(VerdictView {
            kind: VerdictKind::Findings,
            text: "[P1] src/lib.rs:1 -- unbounded retry".into(),
            allowed: vec![
                Resolution::Forwarded,
                Resolution::Dismissed,
                Resolution::Cancelled,
            ],
        }),
    };

    let projected = ViewerTurnReview::from_runtime(&review);

    assert_eq!(projected.tier, "extended");
    assert_eq!(
        projected
            .roles
            .iter()
            .map(|role| (role.label.as_str(), role.state.as_str()))
            .collect::<Vec<_>>(),
        vec![("Supervisor", "done"), ("Tests", "findings")]
    );
    let verdict = projected.verdict.expect("a findings verdict travels");
    assert_eq!(verdict.kind, "findings");
    assert!(verdict.text.contains("unbounded retry"));
    assert_eq!(verdict.allowed, vec!["forward", "dismiss", "cancel"]);
}

/// A phone can always cancel a review, and can only forward or dismiss one
/// the daemon says is ready for it. The same gate runs in the daemon; this
/// one is what makes the refusal immediate.
#[test]
fn resolving_a_review_is_gated_on_what_the_daemon_published() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);

    let resolve = |resolution: &str| ControllerAction::ResolveReview {
        session_id: "session-1".into(),
        resolution: resolution.into(),
    };

    // No review at all.
    let error = validate_action(&resolve("cancel"), &snapshot).unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);

    snapshot.sessions[0].turn_review = Some(ViewerTurnReview {
        tier: "quick".into(),
        status: "the reviewer is reading the change…".into(),
        roles: Vec::new(),
        verdict: None,
    });
    // Running: cancel works, the rest do not.
    validate_action(&resolve("cancel"), &snapshot).unwrap();
    assert_eq!(
        validate_action(&resolve("forward"), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );

    // A failed review can be dismissed but has nothing to forward.
    snapshot.sessions[0].turn_review = Some(ViewerTurnReview {
        tier: "quick".into(),
        status: "the review failed".into(),
        roles: Vec::new(),
        verdict: Some(ViewerReviewVerdict {
            kind: "failed".into(),
            text: "bifrost exited with 1".into(),
            allowed: vec!["dismiss".into(), "cancel".into()],
        }),
    });
    validate_action(&resolve("dismiss"), &snapshot).unwrap();
    assert_eq!(
        validate_action(&resolve("forward"), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
    // A resolution that is not one of the three is refused by name.
    assert_eq!(
        validate_action(&resolve("approve"), &snapshot)
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );

    // Starting a review needs only a session that exists.
    validate_action(
        &ControllerAction::StartReview {
            session_id: "session-1".into(),
        },
        &snapshot,
    )
    .unwrap();
    assert_eq!(
        validate_action(
            &ControllerAction::StartReview {
                session_id: "not-managed".into(),
            },
            &snapshot,
        )
        .unwrap_err()
        .status,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn resume_action_refuses_a_target_the_session_cannot_use() {
    let (mut config, state) = sample_config_state();
    // A project that only exists on GitHub cannot become a checkout on this
    // machine, so the bare target stays out of reach for its sessions.
    config.bundles.get_mut("hel").unwrap().repositories[0].local = None;
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    snapshot.workspaces.push(ViewerWorkspace {
        id: "workspace-1".into(),
        name: "One".into(),
    });
    assert_eq!(
        snapshot.sessions[0].incompatible_resume_targets,
        vec!["raw".to_owned()]
    );

    let error = validate_action(
        &ControllerAction::Resume {
            session_id: "session-1".into(),
            workspace_id: "workspace-1".into(),
            profile_id: "codex-1".into(),
            target_id: "raw".into(),
            queue: ResumeQueueDisposition::Start,
            additional_mounts: None,
            resource_allocation: None,
        },
        &snapshot,
    )
    .unwrap_err();

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
}

#[test]
fn move_confirmation_requires_interruption_ack_and_an_explicit_queue_choice() {
    let (config, state) = sample_config_state();
    let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
    snapshot.sessions[0].capabilities.move_session = true;
    let selection = MoveSelection {
        clear_resource_allocation: false,
        session_id: "session-1".into(),
        profile_id: Some("codex-1".into()),
        target_template_id: Some("podman".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let preparation = MovePreparation {
        in_place: false,
        source_unavailable: false,
        conversion: None,
        selection,
        source_profile_id: "codex-1".into(),
        source_target_template_id: "podman".into(),
        cross_harness: false,
        active: true,
        queued_commands: vec![mj_core::state::MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "command-1".into(),
            kind: mj_core::state::QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "continue"})],
            queued_at_ms: 1,
        }],
        fingerprint: "fingerprint".into(),
        operation_id: "move-1".into(),
    };
    let request = |queue, acknowledge_interruption| MoveSessionRequest {
        preparation: preparation.clone(),
        queue,
        acknowledge_interruption,
    };
    assert_eq!(
        validate_action(
            &ControllerAction::Move {
                request: request(Some(ResumeQueueDisposition::Discard), false),
            },
            &snapshot,
        )
        .unwrap_err()
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        validate_action(
            &ControllerAction::Move {
                request: request(None, true),
            },
            &snapshot,
        )
        .unwrap_err()
        .status,
        StatusCode::BAD_REQUEST
    );
    validate_action(
        &ControllerAction::Move {
            request: request(Some(ResumeQueueDisposition::Discard), true),
        },
        &snapshot,
    )
    .unwrap();
}

#[tokio::test]
async fn snapshot_endpoint_returns_only_public_projection() {
    let (app, _, _, _, _) = app();
    let cookie = login_cookie(&app).await;
    let response = app
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("session-1"));
    assert!(!body.contains("secret-token"));
    assert!(!body.contains("native-secret-id"));
    assert!(!body.contains("/private/source/hel"));

    let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();
    let repository = &snapshot["bundles"][0]["repositories"][0];
    assert_eq!(repository["id"], "hel");
    assert_eq!(repository["github"], "owner/hel");
    assert_eq!(repository["destination"], "hel");
    assert!(repository.get("local").is_none());
}

#[tokio::test]
async fn snapshot_clock_anchor_is_fresh_even_when_the_projection_has_not_changed() {
    let (app, _, _, _, _) = app_with_snapshot(|snapshot| snapshot.server_time_ms = 1);
    let cookie = login_cookie(&app).await;
    for _ in 0..2 {
        let before = mj_core::clock::epoch_millis();
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/snapshot")
                    .header(COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let snapshot: ViewerSnapshot = serde_json::from_slice(&body).unwrap();
        assert!(snapshot.server_time_ms >= before);
        assert!(snapshot.server_time_ms <= mj_core::clock::epoch_millis());
    }
}

#[tokio::test]
async fn conversation_endpoint_returns_authenticated_bounded_deltas() {
    let transcript = BrowserTranscript {
        latest_seq: 8,
        presentation_key: "key-1".into(),
        window_start_seq: 3,
        reset: false,
        entries: vec![
            BrowserTranscriptEntry {
                command_id: None,
                id: 3,
                updated_seq: 3,
                role: "user",
                label: "You".into(),
                recorded_at_ms: None,
                lines: vec!["begin".into()],
                glyph: "\u{276f}",
                tone: "user",
                tool_status: None,
                diffstats: Vec::new(),
            },
            BrowserTranscriptEntry {
                command_id: None,
                id: 7,
                updated_seq: 8,
                role: "agent",
                label: "Agent".into(),
                recorded_at_ms: None,
                lines: vec!["live".into()],
                glyph: "\u{25cf}",
                tone: "agent",
                tool_status: None,
                diffstats: Vec::new(),
            },
        ],
    };
    let (app, _, _, _, _) =
        app_with_conversations(BTreeMap::from([("session-1".into(), transcript)]));
    let cookie = login_cookie(&app).await;
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/conversations/session-1?after_seq=3")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["latest_seq"], 8);
    assert_eq!(body["reset"], false);
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["entries"][0]["lines"][0], "live");

    let response = app
        .oneshot(
            Request::get("/api/conversations/session-1?after_seq=8&presentation_key=stale-key")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["reset"], true);
    assert_eq!(body["presentation_key"], "key-1");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn conversation_endpoint_rejects_cached_transcript_during_transition() {
    let transcript = BrowserTranscript {
        latest_seq: 1,
        presentation_key: "key-1".into(),
        window_start_seq: 1,
        reset: false,
        entries: vec![BrowserTranscriptEntry {
            command_id: None,
            id: 1,
            updated_seq: 1,
            role: "agent",
            label: "Agent".into(),
            recorded_at_ms: None,
            lines: vec!["stale".into()],
            glyph: "●",
            tone: "agent",
            tool_status: None,
            diffstats: Vec::new(),
        }],
    };
    let (app, _, _, _, _) = app_with(
        BTreeMap::from([("session-1".into(), transcript)]),
        |snapshot| snapshot.sessions[0].transitioning = true,
    );
    let cookie = login_cookie(&app).await;
    let response = app
        .oneshot(
            Request::get("/api/conversations/session-1")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn conversation_read_receipt_never_contends_with_a_running_action() {
    let (app, mut actions, mut receipts, _, _) = app();
    let cookie = login_cookie(&app).await;
    // A prompt for the same session stays in flight for the whole test, so
    // a receipt that still travelled the action pipeline would either
    // queue behind it or be rejected for the occupied session slot.
    let prompt = tokio::spawn(
        app.clone().oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"prompt","session_id":"session-1","text":"ship it"}"#,
                ))
                .unwrap(),
        ),
    );
    let action = actions.recv().await.unwrap();

    let response = tokio::spawn(
        app.oneshot(
            Request::post("/api/conversations/session-1/read")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"through":42}"#))
                .unwrap(),
        ),
    );
    let receipt = receipts.recv().await.unwrap();
    assert_eq!(receipt.session_id, "session-1");
    assert_eq!(receipt.through, 42);
    receipt.reply.send(Ok(())).unwrap();
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert!(
        actions.try_recv().is_err(),
        "a read receipt must not queue a controller action"
    );

    action.reply.send(ActionOutcome::accepted()).unwrap();
    assert_eq!(
        prompt.await.unwrap().unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn each_rejected_action_keeps_its_own_status_and_guidance() {
    for (outcome, status, guidance) in [
        (
            ActionOutcome::Busy,
            StatusCode::TOO_MANY_REQUESTS,
            "concurrent action limit",
        ),
        (
            ActionOutcome::SessionBusy,
            StatusCode::CONFLICT,
            "another operation is already running",
        ),
        (
            ActionOutcome::NotCancellable,
            StatusCode::CONFLICT,
            "no cancellable operation",
        ),
        (
            ActionOutcome::Failed {
                reference: "4321-9".to_owned(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "reference 4321-9",
        ),
        (
            ActionOutcome::Refused(mj_core::refusal::Refusal::precondition(
                "this instance has no workspace yet; create one before starting a session",
            )),
            StatusCode::CONFLICT,
            "no workspace yet",
        ),
        (
            ActionOutcome::Refused(mj_core::refusal::Refusal::unusable(
                "no target named laptop is configured",
            )),
            StatusCode::UNPROCESSABLE_ENTITY,
            "no target named laptop",
        ),
    ] {
        let (app, mut actions, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"suspend","session_id":"session-1"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let request = actions.recv().await.unwrap();
        request.reply.send(outcome.clone()).unwrap();

        let response = response.await.unwrap().unwrap();
        assert_eq!(response.status(), status, "{outcome:?}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(guidance), "{outcome:?} answered {error:?}");
    }
}

#[tokio::test]
async fn the_viewer_shows_a_session_whose_action_failed_after_it_was_accepted() {
    // An accepted action reports its outcome only through snapshots, so
    // the application has to react to `has_error` for a late failure to be
    // visible at all.
    let (app, _, _, _, _) = app();
    let script = fetch_text(app, "/viewer.js").await;
    assert!(script.contains("has_error"), "viewer ignores has_error");
}

/// Every response, not only the page, carries the policy. A header that
/// depends on which handler answered is a header somebody will forget.
#[tokio::test]
async fn every_response_carries_the_security_headers() {
    for path in [
        "/",
        "/viewer.js",
        "/voice-worklet.js",
        "/voice-worker.js",
        "/viewer.css",
        "/manifest.webmanifest",
        "/api/snapshot",
    ] {
        let (app, _, _, _, _) = app();
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let headers = response.headers();
        let policy = headers
            .get(CONTENT_SECURITY_POLICY_HEADER)
            .unwrap_or_else(|| panic!("{path} carries no content-security policy"))
            .to_str()
            .unwrap();
        assert!(
            policy.starts_with("default-src 'none';"),
            "{path} does not refuse unlisted sources: {policy}"
        );
        assert!(
            policy.contains("script-src 'self'") && !policy.contains("unsafe-inline"),
            "{path} permits inline script: {policy}"
        );
        assert!(
            policy.contains("frame-ancestors 'none'"),
            "{path} can be framed: {policy}"
        );
        assert_eq!(
            headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff",
            "{path} permits content sniffing"
        );
        assert_eq!(
            headers.get(REFERRER_POLICY).unwrap(),
            "no-referrer",
            "{path} leaks a referrer"
        );
    }
}

/// The policy forbids inline script and style, so the page must contain
/// neither. A page that did would simply fail to run in a browser, which
/// no Rust test would otherwise notice.
#[tokio::test]
async fn the_page_carries_no_inline_script_or_style() {
    let (app, _, _, _, _) = app();
    let page = fetch_text(app, "/").await;
    assert!(
        !page.contains("<script>") && !page.contains("<style>"),
        "the page inlines script or style, which the policy blocks"
    );
    assert!(
        page.contains(r#"src="/viewer.js""#) && page.contains(r#"href="/viewer.css""#),
        "the page does not load its script and style as separate assets"
    );
}

/// A cached API answer is a lie about live session state, and a cached
/// service worker is what keeps a phone on a superseded application.
#[tokio::test]
async fn live_state_and_the_service_worker_are_never_stored() {
    for path in ["/", "/service-worker.js", "/api/snapshot"] {
        let (app, _, _, _, _) = app();
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "no-store",
            "{path} may be stored"
        );
    }
}

/// The worker must leave live state alone entirely rather than caching it
/// and hoping the cache is fresh.
#[test]
fn the_service_worker_declines_to_handle_live_state() {
    assert!(
        SERVICE_WORKER.contains("url.pathname.startsWith('/api/')"),
        "the service worker does not exclude the API"
    );
    assert!(
        SERVICE_WORKER.contains("url.pathname.startsWith('/auth/')"),
        "the service worker does not exclude authentication"
    );
    assert!(
        SERVICE_WORKER.contains("caches.delete"),
        "the service worker never deletes a superseded cache"
    );
}

/// The vendored assets have to reach the browser, not merely exist in the
/// repository: the manifest names them and a phone installs from it.
#[tokio::test]
async fn the_installable_assets_are_served() {
    for (path, content_type) in [
        ("/icon-192.png", "image/png"),
        ("/icon-512.png", "image/png"),
        ("/maskable-512.png", "image/png"),
        ("/apple-touch-icon.png", "image/png"),
        ("/fonts/jetbrains-mono.woff2", "font/woff2"),
    ] {
        let (app, _, _, _, _) = app();
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path} is not served");
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            content_type,
            "{path} is served as the wrong type"
        );
    }
}

/// Fetch one unauthenticated asset and return it as text. Serving the
/// application from several files means a check about the application has
/// to name the file it is about.
async fn fetch_text(app: Router, path: &str) -> String {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path} is not served");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(body.to_vec()).expect("assets are UTF-8")
}

#[tokio::test]
async fn repeated_wrong_codes_lock_the_login_endpoint() {
    let (app, _, _, _, _) = app();
    let attempt = |code: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post("/auth/session")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    for _ in 0..MAX_CODE_FAILURES {
        assert_eq!(attempt("000000").await, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(attempt("000000").await, StatusCode::TOO_MANY_REQUESTS);
    // Even the right code waits out the lockout, so guessing cannot be
    // hidden behind a correct-looking attempt.
    assert_eq!(attempt("123456").await, StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn viewer_code_lockouts_lengthen_instead_of_resetting_after_every_wait() {
    let serve_one_lockout = |guard: &mut CodeGuard, now: Instant| {
        for _ in 0..MAX_CODE_FAILURES {
            assert!(!guard.locked_at(now));
            guard.record_failure_at(now);
        }
        assert!(guard.locked_at(now));
        guard.locked_until.expect("the guard is locked") - now
    };

    let start = Instant::now();
    let mut guard = CodeGuard::default();
    let first = serve_one_lockout(&mut guard, start);
    assert_eq!(first, CODE_LOCKOUT_BASE);

    // Waiting out a lockout buys another run of attempts, not another
    // equally short lockout: a guard that reset here gave an attacker
    // MAX_CODE_FAILURES guesses every CODE_LOCKOUT_BASE for ever.
    let second_round = start + first;
    let second = serve_one_lockout(&mut guard, second_round);
    assert_eq!(second, CODE_LOCKOUT_BASE * 2);
    let third = serve_one_lockout(&mut guard, second_round + second);
    assert_eq!(third, CODE_LOCKOUT_BASE * 4);
    assert_eq!(code_lockout(u32::MAX), CODE_LOCKOUT_CAP);

    // A correct code clears the history, so one mistyped digit tomorrow
    // still costs only the shortest wait.
    let mut recovered = CodeGuard::default();
    assert_eq!(serve_one_lockout(&mut recovered, start), CODE_LOCKOUT_BASE);
}

#[test]
fn persisted_cookie_key_survives_a_restart_and_stays_owner_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("phone-cookie-key");

    let first = load_or_create_cookie_key(&path).unwrap();
    assert!(first.len() >= COOKIE_KEY_BYTES);
    assert_eq!(std::fs::read(&path).unwrap(), first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Two server processes started from the same key file honour each
    // other's cookies; a process that kept its generated key would not.
    let mut restarted = detached_options();
    restarted
        .set_cookie_key(load_or_create_cookie_key(&path).unwrap())
        .unwrap();
    let mut original = detached_options();
    original.set_cookie_key(first.clone()).unwrap();
    let cookie = signed_cookie_value(&original.cookie_key, "test-viewer", 200);
    assert!(session_cookie_valid(&restarted.cookie_key, &cookie, 100));
    assert!(!session_cookie_valid(
        &detached_options().cookie_key,
        &cookie,
        100
    ));

    // Deleting the key file is the explicit sign-everyone-out gesture.
    std::fs::remove_file(&path).unwrap();
    let rotated = load_or_create_cookie_key(&path).unwrap();
    assert_ne!(rotated, first);
    assert!(!session_cookie_valid(&rotated, &cookie, 100));
}

#[test]
fn corrupt_cookie_key_is_regenerated_instead_of_blocking_startup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("phone-cookie-key");
    std::fs::write(&path, b"short").unwrap();

    let key = load_or_create_cookie_key(&path).unwrap();

    assert!(key.len() >= COOKIE_KEY_BYTES);
    assert_eq!(std::fs::read(&path).unwrap(), key);
    assert_eq!(load_or_create_cookie_key(&path).unwrap(), key);
}

#[tokio::test]
async fn bookmarked_qr_login_survives_restart_and_is_revoked_with_the_key() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("phone-cookie-key");
    let mut original = detached_options();
    original
        .set_cookie_key(load_or_create_cookie_key(&path).unwrap())
        .unwrap();
    let url = format!("/auth/login?token={}", original.login_token());
    for _ in 0..2 {
        let mut restarted = detached_options();
        restarted
            .set_cookie_key(load_or_create_cookie_key(&path).unwrap())
            .unwrap();
        let app = router(restarted);
        let response = app
            .clone()
            .oneshot(Request::get(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[LOCATION], "/");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let cookie = response.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let snapshot = app
            .oneshot(
                Request::get("/api/snapshot")
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(snapshot.status(), StatusCode::OK);
    }
    std::fs::remove_file(&path).unwrap();
    let mut rotated = detached_options();
    rotated
        .set_cookie_key(load_or_create_cookie_key(&path).unwrap())
        .unwrap();
    let response = router(rotated)
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response.headers().contains_key(SET_COOKIE));
}

#[tokio::test]
async fn authenticated_requests_renew_cookies_without_changing_viewer_identity() {
    for ttl in [Duration::from_secs(3600), Duration::ZERO] {
        for route in ["/api/snapshot", "/api/v1/sessions"] {
            let mut options = detached_options();
            options.session_ttl = ttl;
            let key = options.cookie_key.clone();
            let now = now_unix();
            let old_expiry = now + 60;
            let viewer = if ttl.is_zero() {
                "session:existing-viewer"
            } else {
                "phone:existing-viewer"
            };
            let old_cookie = signed_cookie_value(&key, viewer, old_expiry);
            let response = router(options)
                .oneshot(
                    Request::get(route)
                        .header(COOKIE, format!("{COOKIE_NAME}={old_cookie}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            let header = response.headers()[SET_COOKIE].to_str().unwrap();
            let renewed = cookie_value(header, COOKIE_NAME).unwrap();
            assert_eq!(
                cookie_viewer(&key, renewed, old_expiry).as_deref(),
                Some(viewer)
            );
            let expiry = renewed.split('.').nth(1).unwrap().parse::<u64>().unwrap();
            let validity = if ttl.is_zero() {
                EPHEMERAL_SESSION_TTL
            } else {
                ttl
            };
            assert!(expiry >= now + validity.as_secs());
            assert!(expiry <= now_unix() + validity.as_secs());
            assert!(header.contains("HttpOnly"));
            assert!(header.contains("SameSite=Strict"));
            assert!(header.contains("Secure"));
            assert_eq!(header.contains("Max-Age="), !ttl.is_zero());
        }
    }
}

#[tokio::test]
async fn rejected_cookies_are_not_renewed_and_bearer_auth_does_not_mint_a_cookie() {
    let mut options = detached_options();
    let key = options.cookie_key.clone();
    options.set_api_token("test-bearer".into());
    let app = router(options);
    for route in ["/api/snapshot", "/api/v1/sessions"] {
        for value in [
            None,
            Some("malformed".to_owned()),
            Some(signed_cookie_value(&key, "expired-viewer", now_unix())),
            Some(signed_cookie_value(
                b"wrong-key",
                "wrong-viewer",
                now_unix() + 3600,
            )),
        ] {
            let mut request = Request::get(route);
            if let Some(value) = value {
                request = request.header(COOKIE, format!("{COOKIE_NAME}={value}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(!response.headers().contains_key(SET_COOKIE));
        }
    }
    let response = app
        .oneshot(
            Request::get("/api/v1/sessions")
                .header(axum::http::header::AUTHORIZATION, "Bearer test-bearer")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(SET_COOKIE));
}

#[tokio::test]
async fn logout_blocks_delayed_renewal_and_rejects_previously_renewed_cookies() {
    let (app, mut bundles) = app_with_bundle_receiver();
    let cookie = login_cookie(&app).await;
    let other_viewer = login_cookie(&app).await;
    let early = app
        .clone()
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let early_cookie = early.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let pending = tokio::spawn(
        app.clone().oneshot(
            Request::post("/api/bundles")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"source":"owner/repo"}"#))
                .unwrap(),
        ),
    );
    let bundle = bundles.recv().await.unwrap();
    let logout = app
        .clone()
        .oneshot(
            Request::delete("/auth/session")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(
        logout.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    bundle
        .reply
        .send(Err(BundleFailure::InvalidSource))
        .unwrap();
    let late = pending.await.unwrap().unwrap();
    assert!(
        !late.headers().contains_key(SET_COOKIE),
        "a delayed response must not renew after logout"
    );
    for route in ["/api/snapshot", "/api/v1/sessions"] {
        for revoked in [&cookie, &early_cookie] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(route)
                        .header(COOKIE, revoked)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(!response.headers().contains_key(SET_COOKIE));
        }
        let response = app
            .clone()
            .oneshot(
                Request::get(route)
                    .header(COOKIE, &other_viewer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn desktop_and_legacy_cookies_keep_their_original_expiry() {
    let options = detached_options();
    let key = options.cookie_key.clone();
    let desktop = mint_desktop_session_cookie(&key).unwrap();
    let legacy = signed_cookie_value(&key, "legacy-viewer", now_unix() + 3600);
    let app = router(options);
    for value in [desktop, legacy] {
        for route in ["/api/snapshot", "/api/v1/sessions"] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(route)
                        .header(COOKIE, format!("{COOKIE_NAME}={value}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(SET_COOKIE));
        }
    }
}

#[tokio::test]
async fn logout_survives_restart_without_revoking_the_bookmarked_login_url() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("phone-cookie-key");
    let mut original = detached_options();
    original
        .load_cookie_credentials(path.clone())
        .await
        .unwrap();
    let url = format!("/auth/login?token={}", original.login_token());
    let app = router(original);
    let login = app
        .clone()
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = login.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let logout = app
        .oneshot(
            Request::delete("/auth/session")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let mut restarted = detached_options();
    restarted.load_cookie_credentials(path).await.unwrap();
    let restarted = router(restarted);
    for route in ["/api/snapshot", "/api/v1/sessions"] {
        let denied = restarted
            .clone()
            .oneshot(
                Request::get(route)
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }
    let fresh = restarted
        .clone()
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(fresh.status(), StatusCode::SEE_OTHER);
    let new_cookie = fresh.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let accepted = restarted
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, new_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn logout_reports_persistence_failure_and_revokes_in_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("phone-cookie-key");
    let mut options = detached_options();
    options.load_cookie_credentials(path).await.unwrap();
    let url = format!("/auth/login?token={}", options.login_token());
    // A directory at the ledger path deterministically refuses the atomic write.
    std::fs::create_dir(directory.path().join("phone-cookie-revocations.json")).unwrap();
    let app = router(options);
    let login = app
        .clone()
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = login.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::delete("/auth/session")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let denied = app
        .clone()
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    std::fs::remove_dir(directory.path().join("phone-cookie-revocations.json")).unwrap();
    let retried = app
        .oneshot(
            Request::delete("/auth/session")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::NO_CONTENT);
    let mut restarted = detached_options();
    restarted
        .load_cookie_credentials(directory.path().join("phone-cookie-key"))
        .await
        .unwrap();
    let denied = router(restarted)
        .oneshot(
            Request::get("/api/snapshot")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn embedded_viewer_displays_quota_recovery_and_unknown_resets() {
    let source = viewer_source(
        "function sessionActivityLabel(",
        "function updateSessionActivity(",
    );
    let setup = "const pendingLifecycleActions = new Map(); function isTransitioningSession() { return false; }";
    let checks = r#"
const session = { lifecycle: 'live', quota_recovery: { retry_at_ms: 120000 } };
if (!sessionActivityLabel(session, 60000).startsWith('Quota limit · resumes ')) throw Error('missing quota deadline');
session.quota_recovery.retry_at_ms = null;
if (sessionActivityLabel(session, 60000) !== 'Quota limit · reset time unknown') throw Error('missing unknown reset');
"#;
    run_viewer_script("quota-recovery", &format!("{setup}\n{source}\n{checks}"));
}
