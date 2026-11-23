//! Daemon-local Wolf Defender Jev inference for the Guardian's approve-only stage.
use crate::github_student::{Notice, PreparationFailure, assets, runtime};
use anyhow::{Context, Result, anyhow, ensure};
use mj_core::guardian::{
    GuardianReviewRequest, WOLF_STAGE1_MAX_CASE_TOKENS, WOLF_STAGE1_OPTIONS, WOLF_STAGE1_QUESTION,
    WolfStage1SkipReason, wolf_scene_packet, wolf_stage1_skip_reason,
};
use ort::{environment::Environment, session::Session, value::Tensor};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokenizers::Tokenizer;
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const MODEL_FILE: &str = "onnx/onnx_fp32/model.onnx";
const TOK_CASE: i64 = 7;
const TOK_QUESTION: i64 = 8;
const TOK_OPTION: i64 = 9;

#[derive(Debug, PartialEq)]
pub enum GuardianStage1Result {
    Scored {
        unsafe_prob: f64,
        tokens: usize,
        elapsed_ms: u64,
    },
    Escalated(WolfStage1SkipReason),
    Overflow {
        tokens: usize,
    },
    Unavailable {
        reason: &'static str,
    },
}

enum State<E> {
    Dormant,
    Loading,
    Ready(Arc<E>),
    Failed { retry_at: Instant },
}

struct Machine<E> {
    state: State<E>,
    failures: u32,
}

impl<E> Machine<E> {
    fn request(&mut self, wake: &Notify) -> Result<(), &'static str> {
        match &self.state {
            State::Ready(_) => return Ok(()),
            State::Loading => return Err("loading"),
            State::Failed { retry_at } if Instant::now() < *retry_at => return Err("load failed"),
            State::Dormant | State::Failed { .. } => {}
        }
        self.state = State::Loading;
        wake.notify_one();
        Err("loading")
    }

    fn loaded(&mut self, result: Result<E>) {
        self.state = match result {
            Ok(engine) => {
                self.failures = 0;
                State::Ready(Arc::new(engine))
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                // Same backoff as the GitHub student, including HTTP Retry-After.
                let delay = Duration::from_secs(1_u64 << self.failures.saturating_sub(1).min(8));
                let delay = error
                    .downcast_ref::<PreparationFailure>()
                    .and_then(|failure| failure.retry_after)
                    .map_or(delay, |hint| delay.max(hint));
                tracing::warn!(target: "mj_guardian", error = %format!("{error:#}"), retry_seconds = delay.as_secs(), "Wolf Defender Jev preparation failed");
                State::Failed {
                    retry_at: Instant::now() + delay,
                }
            }
        };
    }
}

struct Inner {
    machine: Mutex<Machine<Engine>>,
    wake: Notify,
    permits: Arc<Semaphore>,
}

pub struct GuardianClassifier {
    inner: Arc<Inner>,
    cancellation: CancellationToken,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    runtime: tokio::runtime::Handle,
}

impl GuardianClassifier {
    pub fn new(data_directory: PathBuf, use_gpu: bool) -> Self {
        let inner = Arc::new(Inner {
            machine: Mutex::new(Machine {
                state: State::Dormant,
                failures: 0,
            }),
            wake: Notify::new(),
            // ORT requires exclusive access to Run on this session. Never queue
            // behind an active inference: a busy collision escalates immediately
            // to Stage 2 so slot-wait time never counts against a review deadline.
            permits: Arc::new(Semaphore::new(1)),
        });
        let cancellation = CancellationToken::new();
        let supervisor = mj_core::runtime::spawn(supervise(
            inner.clone(),
            data_directory,
            use_gpu,
            cancellation.clone(),
        ));
        Self {
            inner,
            cancellation,
            supervisor: Mutex::new(Some(supervisor)),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub async fn classify(&self, request: &GuardianReviewRequest) -> GuardianStage1Result {
        use GuardianStage1Result as Verdict;

        // Wolf's calibrated packet has no representation of delegated authority.
        if !request.parent_instructions.is_empty() || request.parent_instructions_omitted {
            return Verdict::Unavailable {
                reason: "delegated parent instructions require full review",
            };
        }
        if let Some(reason) = wolf_stage1_skip_reason(request) {
            return Verdict::Escalated(reason);
        }
        let (permit, engine) = match admit_inference(
            &self.inner.machine,
            &self.inner.wake,
            &self.inner.permits,
            &self.cancellation,
        ) {
            Ok(admitted) => admitted,
            Err(reason) => return Verdict::Unavailable { reason },
        };
        let request = request.clone();
        let cancellation = self.cancellation.clone();
        let task = mj_core::runtime::spawn_blocking(move || {
            // The permit stays with the native Run if the requester times out.
            let _permit = permit;
            if cancellation.is_cancelled() {
                return Verdict::Unavailable { reason: "stopped" };
            }
            match engine.score(&request) {
                Ok(verdict) => verdict,
                Err(error) => {
                    tracing::warn!(target: "mj_guardian", %error, "Wolf Defender Jev inference failed");
                    Verdict::Unavailable {
                        reason: "inference failed",
                    }
                }
            }
        });
        tokio::select! {
            _ = self.cancellation.cancelled() => Verdict::Unavailable { reason: "stopped" },
            result = task => match result {
                Ok(verdict) => verdict,
                Err(error) => {
                    tracing::warn!(target: "mj_guardian", %error, "Wolf Defender Jev inference task failed");
                    Verdict::Unavailable { reason: "inference task failed" }
                }
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.cancellation.cancel();
        let task = self
            .supervisor
            .lock()
            .expect("Wolf supervisor poisoned")
            .take();
        if let Some(task) = task {
            task.await.context("Wolf classifier supervisor failed")?;
        }
        self.release_engine();
        Ok(())
    }

    fn release_engine(&self) {
        let state = {
            let mut machine = self
                .inner
                .machine
                .lock()
                .expect("Wolf classifier state poisoned");
            std::mem::replace(&mut machine.state, State::Dormant)
        };
        // Releasing native weights belongs off the daemon event loop.
        if let State::Ready(engine) = state {
            drop(self.runtime.spawn_blocking(move || drop(engine)));
        }
    }
}

impl Drop for GuardianClassifier {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.release_engine();
    }
}

fn admit_inference<E>(
    machine: &Mutex<Machine<E>>,
    wake: &Notify,
    permits: &Arc<Semaphore>,
    cancellation: &CancellationToken,
) -> Result<(OwnedSemaphorePermit, Arc<E>), &'static str> {
    let mut machine = machine.lock().expect("Wolf classifier state poisoned");
    if cancellation.is_cancelled() {
        return Err("stopped");
    }
    machine.request(wake)?;
    let State::Ready(engine) = &machine.state else {
        return Err("loading");
    };
    let permit = permits.clone().try_acquire_owned().map_err(|_| "busy")?;
    Ok((permit, engine.clone()))
}

async fn supervise(
    inner: Arc<Inner>,
    data_directory: PathBuf,
    use_gpu: bool,
    cancellation: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = inner.wake.notified() => {}
        }
        let load = async {
            let (directory, runtime_lib) = prepare(&data_directory, use_gpu, &cancellation).await?;
            let inner = inner.clone();
            let cancellation = cancellation.clone();
            // Publish on the blocking task so a cancelled load can release its
            // native weights here, even when the supervisor has already gone.
            mj_core::runtime::spawn_blocking(move || {
                if cancellation.is_cancelled() {
                    return;
                }
                let loaded = Engine::load(&directory, &runtime_lib, use_gpu);
                let mut machine = inner
                    .machine
                    .lock()
                    .expect("Wolf classifier state poisoned");
                if !cancellation.is_cancelled() {
                    machine.loaded(loaded);
                }
            })
            .await
            .context("Wolf Defender Jev loading task failed")
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = load => result,
        };
        if let Err(error) = result {
            let mut machine = inner
                .machine
                .lock()
                .expect("Wolf classifier state poisoned");
            if !cancellation.is_cancelled() {
                machine.loaded(Err(error));
            }
        }
    }
}

async fn prepare(
    data_directory: &Path,
    use_gpu: bool,
    cancellation: &CancellationToken,
) -> Result<(PathBuf, PathBuf)> {
    runtime::validate_platform()?;
    let manifest = serde_json::from_str(include_str!("guardian_classifier/manifest.json"))?;
    let client = reqwest::Client::builder().build()?;
    let notice: Notice = Arc::new(|notice| {
        tracing::warn!(target: "mj_guardian", ?notice, "Wolf Defender Jev runtime preparation notice");
    });
    tokio::try_join!(
        assets::prepare(
            data_directory.join("models/wolf-defender-jev"),
            manifest,
            "https://huggingface.co",
            &client,
            cancellation,
        ),
        runtime::prepare_selected(
            data_directory.join("github-student-native"),
            use_gpu,
            &notice,
            &client,
            cancellation,
        )
    )
}

#[derive(Deserialize)]
struct ModelConfig {
    max_case_tokens: usize,
    max_question_tokens: usize,
    max_option_tokens: usize,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SpecialToken {
    Text(String),
    Added { content: String },
}

impl SpecialToken {
    fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Added { content: text } => text,
        }
    }
}

#[derive(Deserialize)]
struct TokenizerConfig {
    bos_token: Option<SpecialToken>,
    eos_token: Option<SpecialToken>,
}

struct Prompt {
    bos: i64,
    eos: i64,
    question: Vec<i64>,
    options: Tensor<i64>,
    option_mask: Tensor<i64>,
}

impl Prompt {
    fn load(tokenizer: &Tokenizer, config: ModelConfig, specials: TokenizerConfig) -> Result<Self> {
        ensure!(
            config.max_case_tokens == WOLF_STAGE1_MAX_CASE_TOKENS,
            "Wolf case token limit disagrees with the pinned configuration"
        );
        let special_id = |token: Option<SpecialToken>, default: &str| {
            let text = token.as_ref().map_or(default, SpecialToken::text);
            tokenizer
                .token_to_id(text)
                .map(i64::from)
                .with_context(|| format!("Wolf tokenizer has no {text} token"))
        };
        let bos = special_id(specials.bos_token, "<bos>")?;
        let eos = special_id(specials.eos_token, "<eos>")?;
        let mut question = encode(tokenizer, WOLF_STAGE1_QUESTION)?;
        question.truncate(config.max_question_tokens);
        let options: Vec<Vec<i64>> = WOLF_STAGE1_OPTIONS
            .iter()
            .map(|option| {
                let mut ids = encode(tokenizer, option)?;
                ids.truncate(config.max_option_tokens);
                Ok([&[bos, TOK_OPTION][..], &ids, &[eos]].concat())
            })
            .collect::<Result<_>>()?;
        let width = options
            .iter()
            .map(Vec::len)
            .max()
            .context("no Wolf options")?;
        let shape = [WOLF_STAGE1_OPTIONS.len(), width];
        let mut option_ids = Vec::new();
        let mut option_mask = Vec::new();
        for ids in options {
            option_mask.resize(option_mask.len() + ids.len(), 1);
            option_ids.extend(ids);
            let padded_end = option_ids.len().next_multiple_of(width);
            option_ids.resize(padded_end, 0);
            option_mask.resize(padded_end, 0);
        }
        Ok(Self {
            bos,
            eos,
            question,
            options: Tensor::from_array((shape, option_ids))?,
            option_mask: Tensor::from_array((shape, option_mask))?,
        })
    }
}

fn case_input(bos: i64, eos: i64, case: &[i64], question: &[i64]) -> Option<(Vec<i64>, i64)> {
    if case.len() > WOLF_STAGE1_MAX_CASE_TOKENS {
        return None;
    }
    let ids = [
        &[bos, TOK_CASE][..],
        case,
        &[eos, TOK_QUESTION],
        question,
        &[eos],
    ]
    .concat();
    Some((ids, case.len() as i64 + 3))
}

fn encode(tokenizer: &Tokenizer, text: &str) -> Result<Vec<i64>> {
    let encoded = tokenizer
        .encode(text, false)
        .map_err(|error| anyhow!("Wolf tokenization failed: {error}"))?;
    Ok(encoded.get_ids().iter().copied().map(i64::from).collect())
}

fn unsafe_probability(logits: &[f32]) -> Result<f64> {
    let &[aligned, misaligned] = logits else {
        anyhow::bail!("Wolf returned {} logits; expected two", logits.len());
    };
    ensure!(
        aligned.is_finite() && misaligned.is_finite(),
        "Wolf returned a nonfinite logit"
    );
    let maximum = f64::from(aligned.max(misaligned));
    let aligned = (f64::from(aligned) - maximum).exp();
    let misaligned = (f64::from(misaligned) - maximum).exp();
    Ok(misaligned / (aligned + misaligned))
}

struct Engine {
    tokenizer: Tokenizer,
    prompt: Prompt,
    session: Mutex<Session>,
}

impl Engine {
    fn load(directory: &Path, runtime_lib: &Path, use_gpu: bool) -> Result<Self> {
        let environment = runtime::environment(runtime_lib)?;
        let mut tokenizer = Tokenizer::from_file(directory.join("tokenizer.json"))
            .map_err(|error| anyhow!("could not load Wolf tokenizer: {error}"))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(None)
            .map_err(|error| anyhow!("could not disable Wolf tokenizer truncation: {error}"))?;
        let config = serde_json::from_slice(&std::fs::read(directory.join("config.json"))?)?;
        let specials =
            serde_json::from_slice(&std::fs::read(directory.join("tokenizer_config.json"))?)?;
        let prompt = Prompt::load(&tokenizer, config, specials)?;
        let session = load_session(&environment, &directory.join(MODEL_FILE), use_gpu)?;
        Ok(Self {
            tokenizer,
            prompt,
            session: Mutex::new(session),
        })
    }

    fn score(&self, request: &GuardianReviewRequest) -> Result<GuardianStage1Result> {
        let started = std::time::Instant::now();
        let case = encode(&self.tokenizer, &wolf_scene_packet(request))?;
        let tokens = case.len();
        let Some((ids, question_pos)) = case_input(
            self.prompt.bos,
            self.prompt.eos,
            &case,
            &self.prompt.question,
        ) else {
            return Ok(GuardianStage1Result::Overflow { tokens });
        };
        let len = ids.len();
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("Wolf session poisoned"))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => Tensor::from_array(([1, len], ids))?,
            "attention_mask" => Tensor::from_array(([1, len], vec![1_i64; len]))?,
            "question_pos" => Tensor::from_array(([1], vec![question_pos]))?,
            "option_input_ids" => &self.prompt.options,
            "option_attention_mask" => &self.prompt.option_mask,
        ])?;
        let (shape, logits) = outputs
            .get("logits")
            .context("Wolf graph has no logits output")?
            .try_extract_tensor::<f32>()?;
        ensure!(
            **shape == [1, 2],
            "Wolf returned unexpected logits shape {shape:?}"
        );
        Ok(GuardianStage1Result::Scored {
            unsafe_prob: unsafe_probability(logits)?,
            tokens,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn load_session(environment: &Environment, model: &Path, use_gpu: bool) -> Result<Session> {
    let builder = || -> Result<_> {
        Ok(Session::builder(environment)?
            .with_intra_threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(4)))
            .map_err(ort::Error::<()>::from)?
            .with_inter_threads(1)
            .map_err(ort::Error::<()>::from)?)
    };
    if use_gpu {
        // Use only providers that support Wolf's dynamic input shapes.
        let provider = if runtime::cuda_selected() {
            Some(ort::ep::CUDA::default().with_tf32(false).build())
        } else if runtime::directml_selected() {
            Some(ort::ep::DirectML::default().build())
        } else {
            None
        };
        if let Some(provider) = provider {
            let gpu = || -> Result<Session> {
                Ok(builder()?
                    .with_memory_pattern(false)
                    .map_err(ort::Error::<()>::from)?
                    .with_parallel_execution(false)
                    .map_err(ort::Error::<()>::from)?
                    .with_execution_providers([
                        provider.error_on_failure(),
                        ort::ep::CPU::default().build(),
                    ])
                    .map_err(ort::Error::<()>::from)?
                    .commit_from_file(model)?)
            };
            match gpu() {
                Ok(session) => return Ok(session),
                Err(error) => {
                    tracing::warn!(target: "mj_guardian", %error, "Wolf GPU session failed to load; using CPU")
                }
            }
        }
    }
    Ok(builder()?.commit_from_file(model)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use mj_core::{
        config::HarnessKind,
        guardian::{WOLF_STAGE1_ALLOW_THRESHOLD, WOLF_STAGE1_MAX_ACTION_CHARS},
    };

    fn request(command: &str) -> GuardianReviewRequest {
        GuardianReviewRequest {
            request_id: "request".into(),
            session_id: "session".into(),
            tool_call_id: "tool".into(),
            harness: HarnessKind::Codex,
            action: serde_json::json!({"type": "command", "command": command}),
            user_instructions: vec!["Check the repository status.".into()],
            user_instructions_omitted: false,
            project_instructions: None,
            trusted_user_inputs: Vec::new(),
            parent_instructions: Vec::new(),
            parent_instructions_omitted: false,
            prior_reviews: Vec::new(),
            retry_reason: None,
            transcript: String::new(),
        }
    }

    #[test]
    fn wolf_manifest_pins_the_fp32_graph_and_tokenizer_inputs() {
        let manifest: assets::Manifest =
            serde_json::from_str(include_str!("guardian_classifier/manifest.json")).unwrap();
        for required in [
            "config.json",
            "tokenizer.json",
            "tokenizer_config.json",
            MODEL_FILE,
        ] {
            let file = manifest
                .files
                .iter()
                .find(|file| file.name == required)
                .unwrap();
            assert_eq!(file.sha256.len(), 64);
            assert!(file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(file.bytes > 0);
        }
    }

    #[test]
    fn question_position_points_to_the_question_marker() {
        let (ids, question_pos) = case_input(1, 2, &[42, 43], &[44, 45]).unwrap();
        assert_eq!(ids, [1, 7, 42, 43, 2, 8, 44, 45, 2]);
        assert_eq!(question_pos, 5);
    }

    #[test]
    fn case_token_overflow_skips_sequence_construction_without_truncation() {
        let mut case = vec![42; WOLF_STAGE1_MAX_CASE_TOKENS];
        assert!(case_input(1, 2, &case, &[43]).is_some());
        case.push(42);
        assert!(case_input(1, 2, &case, &[43]).is_none());
    }

    #[test]
    fn softmax_uses_misaligned_logit_and_rejects_nonfinite_output() {
        assert_eq!(unsafe_probability(&[-1000.0, 1000.0]).unwrap(), 1.0);
        assert_eq!(unsafe_probability(&[1000.0, -1000.0]).unwrap(), 0.0);
        assert_eq!(unsafe_probability(&[1000.0, 1000.0]).unwrap(), 0.5);
        assert!(unsafe_probability(&[f32::NAN, 1.0]).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn classifier_retries_failed_loading_only_on_a_request_after_backoff() {
        let mut machine = Machine {
            state: State::Dormant,
            failures: 0,
        };
        let wake = Notify::new();
        assert_eq!(machine.request(&wake), Err("loading"));
        wake.notified().await;
        assert!(matches!(machine.state, State::Loading));
        assert_eq!(machine.request(&wake), Err("loading"));
        assert!(wake.notified().now_or_never().is_none());
        machine.loaded(Err(anyhow!("transient loader failure")));
        assert_eq!(machine.request(&wake), Err("load failed"));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(machine.state, State::Failed { .. }));
        assert_eq!(machine.request(&wake), Err("loading"));
        wake.notified().await;
        machine.loaded(Ok(()));
        assert!(machine.request(&wake).is_ok());
    }

    #[tokio::test]
    async fn oversized_unicode_action_leaves_classifier_dormant() {
        let data_directory = tempfile::tempdir().unwrap();
        let classifier = GuardianClassifier::new(data_directory.path().into(), false);
        let verdict = classifier
            .classify(&request(&"α".repeat(WOLF_STAGE1_MAX_ACTION_CHARS + 1)))
            .await;
        assert_eq!(
            verdict,
            GuardianStage1Result::Escalated(WolfStage1SkipReason::ActionTooLong)
        );
        assert_eq!(
            wolf_stage1_skip_reason(&request(&"α".repeat(WOLF_STAGE1_MAX_ACTION_CHARS))),
            None
        );
        let mut truncated = request("git status");
        truncated.action = serde_json::json!({
            "tool": "Edit",
            "content": [{"path": "a.rs", "patch": "@@\n-a\n+b", "patch_truncated": true}]
        });
        assert_eq!(
            classifier.classify(&truncated).await,
            GuardianStage1Result::Escalated(WolfStage1SkipReason::TruncatedPatch)
        );
        assert!(matches!(
            classifier.inner.machine.lock().unwrap().state,
            State::Dormant
        ));
        classifier.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn guardian_alignment_gates_leave_classifier_dormant() {
        use WolfStage1SkipReason::*;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let classifier = GuardianClassifier::new(dir.path().into(), false);
        let mut request = request("git status");
        for (field, value, reason) in [
            (
                "retry_reason",
                json!("User update arrived"),
                RetryAfterUserUpdate,
            ),
            (
                "trusted_user_inputs",
                json!([{
                    "id": "form", "kind": "manual_permission", "prompt": "Proceed?",
                    "action": "permission_reject",
                }]),
                ManualPermissionRejected,
            ),
            (
                "prior_reviews",
                json!([{
                    "tool": "shell", "action_summary": "make deploy", "outcome": "deny",
                }]),
                RecentPriorDeny,
            ),
        ] {
            let mut wire = serde_json::to_value(&request).unwrap();
            wire[field] = value;
            let gated = serde_json::from_value(wire).unwrap();
            assert_eq!(
                classifier.classify(&gated).await,
                GuardianStage1Result::Escalated(reason)
            );
        }
        request.action["tool_description"] = json!("Untrusted description");
        assert_eq!(
            classifier.classify(&request).await,
            GuardianStage1Result::Escalated(UntrustedActionDescription)
        );
        assert!(matches!(
            classifier.inner.machine.lock().unwrap().state,
            State::Dormant
        ));
        classifier.shutdown().await.unwrap();
    }

    #[test]
    fn busy_inference_slot_escalates_immediately_without_queuing() {
        let machine = Mutex::new(Machine {
            state: State::Ready(Arc::new(())),
            failures: 0,
        });
        let wake = Notify::new();
        let permits = Arc::new(Semaphore::new(1));
        let cancellation = CancellationToken::new();

        let (permit, _) = admit_inference(&machine, &wake, &permits, &cancellation).unwrap();
        assert_eq!(
            admit_inference(&machine, &wake, &permits, &cancellation).unwrap_err(),
            "busy"
        );
        drop(permit);
        assert!(admit_inference(&machine, &wake, &permits, &cancellation).is_ok());
        cancellation.cancel();
        assert_eq!(
            admit_inference(&machine, &wake, &permits, &cancellation).unwrap_err(),
            "stopped"
        );
    }

    #[test]
    #[ignore = "exercises real local Wolf FP32 ONNX weights and ORT runtime"]
    fn wolf_onnx_matches_python_reference_scores() {
        let directory = std::env::var_os("MJ_WOLF_TEST_DIR").map_or_else(
            || {
                PathBuf::from(concat!(
                    "/mnt/optane/tmp/guardian-eval-20261121/hf_cache/hub/",
                    "models--patronus-studio--wolf-defender-jev/snapshots/",
                    "8cd431a3b997cce58d67e3512257ded01258000f"
                ))
            },
            PathBuf::from,
        );
        let runtime = std::env::var_os("MJ_ORT_TEST_LIB").map_or_else(
            || {
                runtime::cached_cpu_library(
                    &mj_core::config::data_dir().join("github-student-native"),
                )
                .expect("set MJ_ORT_TEST_LIB to the pinned ONNX Runtime shared library")
            },
            PathBuf::from,
        );
        let engine = Engine::load(&directory, &runtime, false).unwrap();
        // Reference from the publisher's Python wrapper, FP32 graph on CPU.
        for (command, python_score) in [
            ("git status", 0.0015852842525975804),
            ("rm -rf ~", 0.985115249938432),
        ] {
            let result = engine.score(&request(command)).unwrap();
            let GuardianStage1Result::Scored {
                unsafe_prob,
                tokens,
                elapsed_ms,
            } = result
            else {
                panic!("expected a score, got {result:?}");
            };
            eprintln!(
                "{command}: Rust={unsafe_prob:.17} Python={python_score:.17}, {tokens} tokens, {elapsed_ms} ms"
            );
            assert!((unsafe_prob - python_score).abs() < 5e-6);
            if command == "git status" {
                assert!(unsafe_prob < WOLF_STAGE1_ALLOW_THRESHOLD);
            } else {
                assert!(unsafe_prob > 0.90);
            }
        }

        let mut overflow = request(&"\u{10348}".repeat(WOLF_STAGE1_MAX_ACTION_CHARS));
        overflow.transcript = "\u{10348}".repeat(1500);
        assert!(matches!(
            engine.score(&overflow).unwrap(),
            GuardianStage1Result::Overflow { .. }
        ));
    }
}
