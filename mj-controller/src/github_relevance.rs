//! Frozen student/GLiDE relevance policy. Evidence arrives bounded exactly once.
#![allow(dead_code)] // Concrete model owner is wired when Milestone 3 lands.
use anyhow::{Result, ensure};
use mj_core::github_item::{GithubItemEvidence, GithubItemVerdict};
use serde::Serialize;
use serde_json::json;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

// #1513, accepted labels/panel/band-point90.json operating point.
const NOTIFY: f64 = 0.2830;
const REJECT: f64 = 0.0716;
const GLIDE_NOTIFY: f64 = 0.1123;

pub(crate) type ScoreFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<f64, StudentError>> + Send + 'a>>;
type GlideFuture<'a> = Pin<Box<dyn Future<Output = Result<f64>> + Send + 'a>>;

// These owner types move to crate::github_student when Milestone 3 merges.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StudentFailureKind {
    Download,
    Verification,
    Load,
    Inference,
}
impl StudentFailureKind {
    fn label(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Verification => "verification",
            Self::Load => "load",
            Self::Inference => "inference",
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) enum StudentState {
    Loading,
    Ready,
    Failed {
        kind: StudentFailureKind,
        cause: String,
        next_retry_at_ms: i64,
    },
}

// Moves to crate::github_student::StudentError when Milestone 3 merges.
#[derive(Debug)]
pub(crate) enum StudentError {
    Unavailable(StudentState),
    Inference(anyhow::Error),
}
impl std::fmt::Display for StudentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(state) => write!(f, "student unavailable: {state:?}"),
            Self::Inference(error) => write!(f, "student inference failed: {error:#}"),
        }
    }
}
impl std::error::Error for StudentError {}

/// Readiness, preparation/retry and inference concurrency belong to this owner.
/// Scoring must run blocking inference off the async runtime.
pub(crate) trait StudentOwner: Send + Sync {
    fn state(&self) -> StudentState;
    fn manifest_revision(&self) -> &str;
    fn score_segments<'a>(&'a self, item: &'a str, session: &'a str) -> ScoreFuture<'a>;
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub(crate) enum Route {
    StudentNotify,
    StudentReject,
    GlideBand,
    GlideDegraded,
}

pub(crate) struct Classifier {
    student: Arc<dyn StudentOwner>,
    degraded: Mutex<bool>,
    notice: Box<dyn Fn(String) + Send + Sync>,
}
impl Classifier {
    pub(crate) fn new(
        student: Arc<dyn StudentOwner>,
        notice: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self {
            student,
            degraded: Mutex::new(false),
            notice: Box::new(notice),
        }
    }

    pub(crate) fn observe(&self, failure: Option<(Option<StudentFailureKind>, &str)>) {
        let mut degraded = self.degraded.lock().unwrap_or_else(|p| p.into_inner());
        match (failure, *degraded) {
            (Some((kind, cause)), false) => {
                *degraded = true;
                tracing::warn!(cause, "GitHub local relevance scoring unavailable");
                let stage = kind.map_or("preparation", StudentFailureKind::label);
                (self.notice)(format!(
                    "Local GitHub relevance scoring is unavailable ({stage}). Every GitHub item is scored by GLiDE while the model retries automatically."
                ));
            }
            (None, true) => {
                *degraded = false;
                (self.notice)("GitHub relevance local model recovered.".into());
            }
            _ => {}
        }
    }

    pub(crate) async fn classify(
        &self,
        evidence: &GithubItemEvidence,
    ) -> Result<GithubItemVerdict> {
        self.classify_with(evidence, |e| {
            Box::pin(crate::github_item_verdict::classify_probability(e))
        })
        .await
    }

    async fn classify_with<F>(
        &self,
        evidence: &GithubItemEvidence,
        glide: F,
    ) -> Result<GithubItemVerdict>
    where
        F: for<'a> FnOnce(&'a GithubItemEvidence) -> GlideFuture<'a>,
    {
        let started = Instant::now();
        let attempt = match mj_core::jev::DecisionLog::open(mj_core::jev::controller_log_dir()) {
            Ok(log) => Some(log.start(
                &format!("github:{}#{}", evidence.item.repo, evidence.item.number),
                "github-relevance",
                "Is this GitHub item relevant to this session?",
                "Student with GLiDE middle band",
            )),
            Err(error) => {
                tracing::warn!(%error, "GitHub relevance diagnostic log unavailable");
                None
            }
        };
        if let Some(attempt) = &attempt {
            attempt.update(
                Some("Waiting for the local model or GLiDE."),
                json!({"manifest_revision": self.student.manifest_revision()}),
            );
        }
        let mut failure = None;
        let mut failure_kind = None;
        let (item, session) = mj_transcript::github_history::student_segments(evidence)?;
        // Scoring is the owner's atomic decision point and starts lazy preparation.
        let student_probability = match self
            .student
            .score_segments(&item, &session)
            .await
            .and_then(|p| valid_probability(p).map_err(StudentError::Inference))
        {
            Ok(p) => Some(p),
            Err(StudentError::Unavailable(StudentState::Failed {
                kind,
                cause,
                next_retry_at_ms,
            })) => {
                failure_kind = Some(kind);
                failure = Some(format!(
                    "{cause}; automatic retry at epoch milliseconds {next_retry_at_ms}"
                ));
                None
            }
            Err(StudentError::Unavailable(state)) => {
                failure = Some(format!(
                    "model unavailable ({state:?}); automatic preparation is running"
                ));
                None
            }
            Err(StudentError::Inference(error)) => {
                failure_kind = Some(StudentFailureKind::Inference);
                failure = Some(format!("{error:#}; model owner will retry preparation"));
                tracing::warn!(%error, "GitHub student inference failed; using GLiDE");
                None
            }
        };
        self.observe(failure.as_deref().map(|cause| (failure_kind, cause)));
        let route = match student_probability {
            Some(s) if s >= NOTIFY => Route::StudentNotify,
            Some(s) if s < REJECT => Route::StudentReject,
            Some(_) => Route::GlideBand,
            None => Route::GlideDegraded,
        };
        let mut glide_probability = None;
        let result = match route {
            Route::StudentNotify => Ok(GithubItemVerdict { interested: true }),
            Route::StudentReject => Ok(GithubItemVerdict { interested: false }),
            _ => match glide(evidence).await.and_then(valid_probability) {
                Ok(p) => {
                    glide_probability = Some(p);
                    Ok(GithubItemVerdict {
                        interested: p >= GLIDE_NOTIFY,
                    })
                }
                Err(error) => Err(error),
            },
        };
        if let Some(attempt) = attempt {
            attempt.update(None, json!({"student_probability":student_probability, "glide_probability":glide_probability, "manifest_revision":self.student.manifest_revision(), "route":route, "elapsed_ms":started.elapsed().as_millis(), "failure_context":failure, "error":result.as_ref().err().map(|e| format!("{e:#}"))}));
            match &result {
                Ok(verdict) => {
                    attempt.update(
                        Some(&format!("Interested: {}.", verdict.interested)),
                        json!({}),
                    );
                    attempt.finish("classified", "GitHub relevance classified this item.");
                }
                Err(_) => attempt.finish(
                    "failed",
                    "GitHub relevance failed; durable retry remains pending.",
                ),
            }
        }
        result
    }
}
fn valid_probability(p: f64) -> Result<f64> {
    ensure!(
        p.is_finite() && (0.0..=1.0).contains(&p),
        "invalid relevance probability: {p}"
    );
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct RecordedStudent {
        state: Mutex<StudentState>,
        score: f64,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl StudentOwner for RecordedStudent {
        fn state(&self) -> StudentState {
            panic!("policy must not poll readiness before scoring")
        }
        fn manifest_revision(&self) -> &str {
            "recorded-boundary"
        }
        fn score_segments<'a>(&'a self, item: &'a str, session: &'a str) -> ScoreFuture<'a> {
            Box::pin(async move {
                assert!(serde_json::from_str::<serde_json::Value>(item).is_ok());
                assert!(
                    serde_json::from_str::<serde_json::Value>(session).unwrap()["history"]
                        .is_array()
                );
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match self.state.lock().unwrap().clone() {
                    StudentState::Ready => Ok(self.score),
                    state => Err(StudentError::Unavailable(state)),
                }
            })
        }
    }
    fn fixture() -> GithubItemEvidence {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../services/jev-proxy/tests/fixtures/github-item-aliases600.json"
        ))
        .unwrap();
        serde_json::from_value(fixture["evidence"].clone()).unwrap()
    }
    // Hard-won: #1513: a readiness pre-check left lazy model preparation dormant.
    #[tokio::test]
    async fn github_relevance_exact_boundaries_and_failure_episodes() {
        const CHILD: &str = "MJ_GITHUB_RELEVANCE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "github_relevance_exact_boundaries_and_failure_episodes",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }
        // Threshold ordering and failure recovery: an error must never become a rejection.
        let evidence = fixture();
        for (score, provider, interested, calls) in [
            (0.2830, 0.0, true, 0),
            (0.2830 - f64::EPSILON, 0.1123, true, 1),
            (0.0716, 0.1123 - f64::EPSILON, false, 1),
            (0.0716 - f64::EPSILON, 1.0, false, 0),
            (f64::NAN, 0.1123, true, 1),
        ] {
            let student = Arc::new(RecordedStudent {
                state: Mutex::new(StudentState::Ready),
                score,
                calls: Default::default(),
            });
            let classifier = Classifier::new(student, |_| {});
            let used = std::sync::atomic::AtomicUsize::new(0);
            let result = classifier
                .classify_with(&evidence, |_| {
                    used.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async move { Ok(provider) })
                })
                .await
                .unwrap();
            assert_eq!(result.interested, interested);
            assert_eq!(used.load(std::sync::atomic::Ordering::SeqCst), calls);
        }
        let student = Arc::new(RecordedStudent {
            state: Mutex::new(StudentState::Loading),
            score: 0.5,
            calls: Default::default(),
        });
        let notices = Arc::new(Mutex::new(Vec::new()));
        let captured = notices.clone();
        let classifier =
            Classifier::new(student.clone(), move |s| captured.lock().unwrap().push(s));
        for _ in 0..2 {
            assert!(
                classifier
                    .classify_with(&evidence, |_| Box::pin(async {
                        anyhow::bail!("provider unavailable")
                    }))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            student.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "Loading must still call score_segments to start preparation"
        );
        *student.state.lock().unwrap() = StudentState::Failed {
            kind: StudentFailureKind::Download,
            cause: "download failed".into(),
            next_retry_at_ms: 123,
        };
        assert!(
            classifier
                .classify_with(&evidence, |_| Box::pin(async { Ok(0.1123) }))
                .await
                .unwrap()
                .interested
        );
        assert_eq!(notices.lock().unwrap().len(), 1);
        *student.state.lock().unwrap() = StudentState::Ready;
        assert!(
            classifier
                .classify_with(&evidence, |_| Box::pin(async {
                    panic!("unexpected provider")
                }))
                .await
                .unwrap()
                .interested
        );
        assert_eq!(notices.lock().unwrap().len(), 2);
    }
}
