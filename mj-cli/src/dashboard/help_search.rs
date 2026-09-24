//! One cancellable help lookup at a time, owned by the terminal dashboard.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use mj_core::help_search::{HelpSearchRequest, HelpSearchResponse, MAX_BODY_BYTES};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::task::AbortOnDropHandle;

use super::io::{DashboardIoUpdate, report};

const DEBOUNCE: Duration = Duration::from_millis(200);
const DEADLINE: Duration = Duration::from_secs(10);
const HOSTED: &str = "https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/help-search";
const DIRECT: &str = "https://api.typesafe.ai/v1/systemone";

#[derive(Default)]
pub(super) struct HelpSearch {
    generation: Option<u64>,
    task: Option<AbortOnDropHandle<()>>,
}

impl HelpSearch {
    pub(super) fn sync(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        updates: &UnboundedSender<DashboardIoUpdate>,
    ) {
        let generation = dashboard.help_search_generation();
        if self.generation == generation {
            return;
        }
        self.cancel();
        self.generation = generation;
        if let (Some(generation), Some(request)) = (generation, dashboard.help_search_request()) {
            self.task = Some(start(generation, updates.clone(), async move {
                request.validate()?;
                let key = tokio::task::spawn_blocking(mj_core::activity::verdict::api_key)
                    .await
                    .context("resolve help search credentials")?;
                search(
                    &request,
                    if key.is_some() { DIRECT } else { HOSTED },
                    key.as_deref(),
                )
                .await
            }));
        }
    }

    pub(super) fn cancel(&mut self) {
        self.task = None;
        self.generation = None;
    }
}

fn start(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl std::future::Future<Output = Result<HelpSearchResponse>> + Send + 'static,
) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(async move {
        tokio::time::sleep(DEBOUNCE).await;
        let child = AbortOnDropHandle::new(tokio::spawn(work));
        let result = match tokio::time::timeout(DEADLINE, child).await {
            Ok(Ok(result)) => result.map_err(|error| format!("{error:#}")),
            Ok(Err(_)) => Err("help search task failed".to_owned()),
            Err(_) => Err("help search timed out".to_owned()),
        };
        if let Err(error) = &result {
            tracing::warn!(%error, "semantic help search unavailable");
        }
        report(
            "help search",
            &updates,
            DashboardIoUpdate::HelpSearchFinished { generation, result },
        );
    }))
}

async fn search(
    request: &HelpSearchRequest,
    endpoint: &str,
    key: Option<&str>,
) -> Result<HelpSearchResponse> {
    request.validate()?;
    let client = reqwest::Client::builder()
        .timeout(DEADLINE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("create help search client")?;
    let http = client.post(endpoint);
    let http = match key {
        Some(key) => http.bearer_auth(key).json(&request.upstream_body()),
        None => http.json(request),
    };
    let mut response = http
        .send()
        .await
        .context("request help search")?
        .error_for_status()
        .context("help search HTTP status")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_BODY_BYTES as u64),
        "help response exceeds byte limit"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read help search")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_BODY_BYTES,
            "help response exceeds byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    let result = if key.is_some() {
        request.parse_upstream(&serde_json::from_slice(&body).context("decode help answers")?)?
    } else {
        serde_json::from_slice::<HelpSearchResponse>(&body).context("decode help scores")?
    };
    result.validate(request)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::help_search::{HelpSearchEntry, HelpSearchScore};
    use tokio::sync::mpsc::unbounded_channel;

    fn request() -> HelpSearchRequest {
        HelpSearchRequest {
            query: "leave agents running".into(),
            entries: vec![HelpSearchEntry {
                id: 0,
                category: "Essentials".into(),
                label: "Detach".into(),
                description: "Keep sessions running".into(),
            }],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lookup_debounces_cancels_and_reports_deadlines_and_panics() {
        let (tx, mut rx) = unbounded_channel();
        let task = start(1, tx.clone(), async {
            Ok(HelpSearchResponse { scores: vec![] })
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(199)).await;
        assert!(rx.try_recv().is_err());
        drop(task);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(rx.try_recv().is_err());
        let task = start(2, tx.clone(), async {
            Ok(HelpSearchResponse { scores: vec![] })
        });
        let began = tokio::time::Instant::now();
        let update = rx.recv().await.unwrap();
        assert_eq!(began.elapsed(), DEBOUNCE);
        assert!(matches!(
            update,
            DashboardIoUpdate::HelpSearchFinished {
                generation: 2,
                result: Ok(_)
            }
        ));
        task.await.unwrap();
        let task = start(3, tx.clone(), std::future::pending());
        assert!(matches!(
            rx.recv().await.unwrap(),
            DashboardIoUpdate::HelpSearchFinished {
                generation: 3,
                result: Err(_)
            }
        ));
        task.await.unwrap();
        let task = start(4, tx, async { panic!("test task panic") });
        assert!(matches!(
            rx.recv().await.unwrap(),
            DashboardIoUpdate::HelpSearchFinished {
                generation: 4,
                result: Err(_)
            }
        ));
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_an_inflight_lookup_drops_its_work_without_a_late_result() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let (started, start_rx) = tokio::sync::oneshot::channel();
        let (tx, mut rx) = unbounded_channel();
        let task = start(1, tx, async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending().await
        });
        start_rx.await.unwrap();
        assert!(!dropped.load(Ordering::SeqCst));
        drop(task);
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(dropped.load(Ordering::SeqCst));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn direct_and_hosted_http_validate_scores_and_handle_failures() {
        use axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::post,
        };
        use serde_json::{Value, json};
        async fn handler(
            State((direct, status, body)): State<(bool, StatusCode, Value)>,
            headers: HeaderMap,
            Json(request): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            if direct {
                assert_eq!(headers["authorization"], "Bearer test-key");
                assert_eq!(request["model"], "jev-latest");
                assert!(
                    request["questions"]["entry_0"]["instructions"]
                        .as_str()
                        .unwrap()
                        .contains("entries[0]")
                );
            } else {
                assert!(!headers.contains_key("authorization"));
                assert!(request.get("questions").is_none());
                assert_eq!(request["query"], "leave agents running");
            }
            (status, Json(body))
        }
        for (direct, status, body, valid) in [
            (
                false,
                StatusCode::OK,
                json!({"scores":[{"id":0,"probability":0.9}]}),
                true,
            ),
            (
                true,
                StatusCode::OK,
                json!({"answers":{"entry_0":{"type":"noul","noul":0.9}}}),
                true,
            ),
            (
                false,
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":"rate_limited"}),
                false,
            ),
            (false, StatusCode::OK, json!({"scores":[]}), false),
            (
                false,
                StatusCode::OK,
                json!({"scores":[{"id":0,"probability":2}]}),
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/", post(handler))
                .with_state((direct, status, body));
            let server = AbortOnDropHandle::new(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap()
            }));
            let result = search(&request(), &endpoint, direct.then_some("test-key")).await;
            assert_eq!(result.is_ok(), valid, "{result:?}");
            if valid {
                assert_eq!(
                    result.unwrap().scores,
                    vec![HelpSearchScore {
                        id: 0,
                        probability: 0.9
                    }]
                );
            }
            drop(server);
        }
    }
}
