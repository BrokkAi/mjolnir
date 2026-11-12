use super::*;
use serde::Deserialize;
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Deserialize)]
struct Pair {
    identity: serde_json::Value,
    item: String,
    session: String,
    input_ids: Vec<i64>,
    attention_mask: Vec<i64>,
    python_fp32: f64,
}
fn pairs() -> Vec<Pair> {
    include_str!("../../tests/fixtures/github-student/parity.jsonl")
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn cache() -> PathBuf {
    let path = std::env::var_os("MJ_GITHUB_STUDENT_TEST_CACHE")
        .map(PathBuf::from)
        .expect("missing verified model/native cache; see tests/fixtures/github-student/README.md");
    if std::env::var_os("MJ_GITHUB_STUDENT_PREPARE").is_some() {
        std::fs::create_dir_all(&path).unwrap();
        return path;
    }
    assert!(
        path.join("github-student")
            .join(&assets::Manifest::pinned().revision)
            .is_dir(),
        "missing pinned model revision in {}",
        path.display()
    );
    assert!(
        path.join("github-student-native").is_dir(),
        "missing pinned native runtime in {}",
        path.display()
    );
    path
}
async fn wait_for(student: &Student, predicate: impl Fn(&StudentState) -> bool) -> StudentState {
    if matches!(student.inner.machine.lock().unwrap().state, State::Dormant) {
        assert!(
            student
                .score_segments("first real classification request", "session")
                .await
                .is_err()
        );
    }
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let notified = student.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = student.state();
            if predicate(&state) {
                return state;
            }
            notified.await;
        }
    })
    .await
    .expect("student state transition did not finish")
}
fn band(score: f64) -> u8 {
    if score < 0.0716 {
        0
    } else if score < 0.2830 {
        1
    } else {
        2
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the verified pinned model/runtime cache; missing assets fail explicitly"]
async fn python_parity_and_sixteen_concurrent_calls_share_one_loaded_session() {
    crate::server::install_rustls_crypto_provider();
    let student = Student::new(
        cache(),
        false,
        Arc::new(|_| panic!("CPU-only scorer must not select a GPU")),
    );
    let state = wait_for(&student, |s| !matches!(s, StudentState::Loading)).await;
    assert_eq!(state, StudentState::Ready, "{state:?}");
    let cases = pairs();
    let mut serial = Vec::new();
    let engine = {
        let machine = student.inner.machine.lock().unwrap();
        let State::Ready(ready) = &machine.state else {
            panic!("student not ready")
        };
        ready.engine.clone()
    };
    for pair in &cases {
        let (ids, mask) = engine.tokens(&pair.item, &pair.session).unwrap();
        assert_eq!(
            ids, pair.input_ids,
            "{}: first divergence is token IDs",
            pair.identity
        );
        assert_eq!(
            mask, pair.attention_mask,
            "{}: first divergence is masks",
            pair.identity
        );
        let score = student
            .score_segments(&pair.item, &pair.session)
            .await
            .unwrap();
        assert!(
            (score - pair.python_fp32).abs() <= 0.005,
            "{}: score {score} vs Python {}",
            pair.identity,
            pair.python_fp32
        );
        let near = (pair.python_fp32 - 0.0716).abs() <= 0.005
            || (pair.python_fp32 - 0.2830).abs() <= 0.005;
        assert!(
            band(score) == band(pair.python_fp32) || near,
            "{}: routing changed outside near-cut margin",
            pair.identity
        );
        serial.push(score);
    }
    let overflow: Pair = serde_json::from_str(include_str!(
        "../../tests/fixtures/github-student/overflow.json"
    ))
    .unwrap();
    let (ids, mask) = engine.tokens(&overflow.item, &overflow.session).unwrap();
    assert_eq!(
        ids, overflow.input_ids,
        "overflow: first divergence is token IDs"
    );
    assert_eq!(
        mask, overflow.attention_mask,
        "overflow: first divergence is masks"
    );
    let score = student
        .score_segments(&overflow.item, &overflow.session)
        .await
        .unwrap();
    assert!(
        (score - overflow.python_fp32).abs() <= 0.005,
        "overflow score {score} vs Python {}",
        overflow.python_fp32
    );
    let concurrent = futures::future::join_all(
        cases[..16]
            .iter()
            .map(|p| student.score_segments(&p.item, &p.session)),
    )
    .await;
    for (i, result) in concurrent.into_iter().enumerate() {
        assert_eq!(
            result.unwrap().to_bits(),
            serial[i].to_bits(),
            "{}: serial/concurrent bits differ",
            cases[i].identity
        );
    }
    student.shutdown().await.unwrap();
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let to = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires verified pinned model/runtime assets for actual ORT loader recovery"]
async fn owner_reuses_loaded_cpu_across_health_and_preference_changes() {
    use axum::{
        Router,
        body::Body,
        extract::{Path as HttpPath, State as HttpState},
        http::StatusCode,
        response::Response,
        routing::get,
    };
    crate::server::install_rustls_crypto_provider();
    #[derive(Clone)]
    struct Source {
        root: PathBuf,
        requests: Arc<AtomicUsize>,
    }
    async fn serve(
        HttpState(source): HttpState<Source>,
        HttpPath(path): HttpPath<String>,
    ) -> Response {
        source.requests.fetch_add(1, Ordering::SeqCst);
        let name = Path::new(&path).file_name().unwrap();
        match tokio::fs::File::open(source.root.join(name)).await {
            Ok(file) => Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(file))),
            Err(error) => {
                tracing::warn!(%error,"student model fixture read failed");
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap()
            }
        }
    }
    let cache = cache();
    let root = tempfile::Builder::new()
        .prefix("github-student-owner-")
        .tempdir()
        .unwrap();
    copy_tree(
        &cache.join("github-student-native"),
        &root.path().join("github-student-native"),
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let source = Source {
        root: cache
            .join("github-student")
            .join(&assets::Manifest::pinned().revision),
        requests: requests.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/{*path}", get(serve))
                .with_state(source),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
    });
    let notices = Arc::new(Mutex::new(Vec::new()));
    let recorded = notices.clone();
    let student = Student::start(
        root.path().into(),
        false,
        Arc::new(move |notice| recorded.lock().unwrap().push(notice)),
        PreparationOptions {
            model_base: base,
            model_manifest: assets::Manifest::pinned().clone(),
            load_failures: Some(Arc::new(AtomicUsize::new(1))),
        },
    );
    tokio::task::yield_now().await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "constructor and status must not prepare"
    );
    assert!(!root.path().join("github-student").exists());
    assert_eq!(student.state(), StudentState::Loading);
    let unavailable =
        futures::future::join_all((0..16).map(|_| student.score_segments("item", "session"))).await;
    assert!(unavailable.into_iter().all(|r| r.is_err()));
    let failure = wait_for(&student, |s| matches!(s, StudentState::Failed { .. })).await;
    assert!(
        matches!(
            failure,
            StudentState::Failed {
                kind: StudentFailureKind::Load,
                ..
            }
        ),
        "{failure:?}"
    );
    let state = wait_for(&student, |s| matches!(s, StudentState::Ready)).await;
    assert_eq!(state, StudentState::Ready);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        assets::Manifest::pinned().selected(false).files.len(),
        "retry must reuse verified assets; callers must not independently prepare"
    );
    let (engine, permits) = {
        let machine = student.inner.machine.lock().unwrap();
        let State::Ready(ready) = &machine.state else {
            panic!("student not ready")
        };
        (ready.engine.clone(), ready.permits.clone())
    };
    let admitted = permits.acquire_owned().await.unwrap();
    student
        .inner
        .completed(&engine, &Err(anyhow::anyhow!("recorded inference failure")));
    assert_eq!(student.state(), StudentState::Ready);
    student.inner.completed(
        &engine,
        &Err(anyhow::anyhow!("second failure while unhealthy")),
    );
    {
        let machine = student.inner.machine.lock().unwrap();
        let State::Ready(ready) = &machine.state else {
            panic!("lost readiness")
        };
        assert!(ready.failing);
        assert!(Arc::ptr_eq(&engine, &ready.engine));
    }
    student.inner.completed(&engine, &Ok(0.5));
    {
        let notices = notices.lock().unwrap();
        assert_eq!(
            notices
                .iter()
                .filter(|n| matches!(n, StudentNotice::InferenceFailing { .. }))
                .count(),
            1
        );
        assert_eq!(
            notices
                .iter()
                .filter(|n| matches!(n, StudentNotice::InferenceRecovered))
                .count(),
            1
        );
    }
    student.set_use_gpu(true);
    student.set_use_gpu(false);
    {
        let machine = student.inner.machine.lock().unwrap();
        let State::Ready(ready) = &machine.state else {
            panic!("lost readiness")
        };
        assert!(!ready.failing);
        assert!(Arc::ptr_eq(&engine, &ready.engine));
        assert!(matches!(ready.device, Device::Cpu));
    }
    drop(admitted);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        assets::Manifest::pinned().selected(false).files.len()
    );
    student.shutdown().await.unwrap();
    cancel.cancel();
    server.await.unwrap().unwrap();
}

/// Two independently exported static graphs must agree with Python, including
/// only_second overflow. On a Mac this also proves real CoreML placement.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads optional CoreML graphs; runs real static graph inference"]
async fn coreml_external_initializers_padding_and_python_parity() {
    crate::server::install_rustls_crypto_provider();
    let root = cache();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let directory = assets::prepare(
        root.join("github-student"),
        assets::Manifest::pinned().selected(true),
        "https://huggingface.co",
        &client,
        &cancel,
    )
    .await
    .unwrap();
    let runtime = runtime::prepare_selected(
        root.join("github-student-native"),
        false,
        &(Arc::new(|_| {}) as Notice),
        &client,
        &cancel,
    )
    .await
    .unwrap();
    let engine = inference::Engine::load(&directory, &runtime).unwrap();
    let accelerated = cfg!(all(target_os = "macos", target_arch = "aarch64"));
    let mut model = engine.load_coreml(&directory, accelerated).unwrap();
    let cases = pairs();
    let overflow: Pair = serde_json::from_str(include_str!(
        "../../tests/fixtures/github-student/overflow.json"
    ))
    .unwrap();
    for pair in [
        cases.iter().min_by_key(|p| p.input_ids.len()).unwrap(),
        &overflow,
    ] {
        let score = model
            .score(pair.input_ids.clone(), pair.attention_mask.clone())
            .unwrap();
        assert!(
            (score - pair.python_fp32).abs() <= 0.005,
            "{}: {score} vs {}",
            pair.identity,
            pair.python_fp32
        );
        assert!(
            band(score) == band(pair.python_fp32)
                || (pair.python_fp32 - 0.0716).abs() <= 0.005
                || (pair.python_fp32 - 0.283).abs() <= 0.005
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a host GPU; deliberately fails if its real EP cannot run"]
async fn actual_gpu_python_parity_and_warm_latency() {
    crate::server::install_rustls_crypto_provider();
    let root = cache();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let runtime = runtime::prepare_selected(
        root.join("github-student-native"),
        true,
        &(Arc::new(|notice| eprintln!("{notice:?}")) as Notice),
        &client,
        &cancel,
    )
    .await
    .unwrap();
    let initial = assets::prepare(
        root.join("github-student"),
        assets::Manifest::pinned().selected(false),
        "https://huggingface.co",
        &client,
        &cancel,
    )
    .await
    .unwrap();
    let engine = inference::Engine::load(&initial, &runtime).unwrap();
    let device = engine
        .preferred_device()
        .expect("host GPU libraries unavailable");
    let directory = assets::prepare(
        root.join("github-student"),
        assets::Manifest::pinned().selected(device == StudentDevice::CoreMl),
        "https://huggingface.co",
        &client,
        &cancel,
    )
    .await
    .unwrap();
    let gpu = engine.load_gpu(&directory, device).unwrap();
    let mut session = gpu.session.lock().unwrap();
    let cases = pairs();
    session
        .warmup(cases[0].input_ids.clone(), cases[0].attention_mask.clone())
        .unwrap();
    let mut cpu_seconds = 0.0;
    let mut gpu_seconds = 0.0;
    let mut max_error = 0.0_f64;
    for (index, pair) in cases.iter().enumerate() {
        let start = std::time::Instant::now();
        let score = session
            .score(pair.input_ids.clone(), pair.attention_mask.clone())
            .unwrap();
        gpu_seconds += start.elapsed().as_secs_f64();
        max_error = max_error.max((score - pair.python_fp32).abs());
        assert!(
            (score - pair.python_fp32).abs() <= 0.005,
            "{}: {score} vs {}",
            pair.identity,
            pair.python_fp32
        );
        assert!(
            band(score) == band(pair.python_fp32)
                || (pair.python_fp32 - 0.0716).abs() <= 0.005
                || (pair.python_fp32 - 0.283).abs() <= 0.005
        );
        if index < 5 {
            let start = std::time::Instant::now();
            engine
                .score_tokens(pair.input_ids.clone(), pair.attention_mask.clone())
                .unwrap();
            cpu_seconds += start.elapsed().as_secs_f64();
        }
    }
    eprintln!(
        "device={device:?} pairs={} max_error={max_error} warm_gpu_mean_seconds={} cpu_first5_mean_seconds={}",
        cases.len(),
        gpu_seconds / cases.len() as f64,
        cpu_seconds / 5.0
    );
}
