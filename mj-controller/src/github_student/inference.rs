//! Tokenization and inference. These entry points run on supervised blocking work.
use super::StudentDevice;
use anyhow::{Context, Result, bail, ensure};
use ort::{environment::Environment, session::Session, value::Tensor};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokenizers::{Tokenizer, TruncationParams, TruncationStrategy};

fn profile_prefix(directory: &Path) -> Result<PathBuf> {
    // ORT appends its timestamp and .json suffix; reserve a unique prefix.
    Ok(tempfile::Builder::new()
        .prefix("placement-")
        .tempfile_in(directory)?
        .path()
        .to_path_buf())
}
pub(super) struct Engine {
    pub(super) cpu: Session,
    environment: Environment,
    cuda: bool,
    tokenizer: Tokenizer,
}
pub(super) struct Gpu {
    pub device: StudentDevice,
    pub session: Mutex<GpuSessions>,
}
impl Engine {
    pub fn load(directory: &Path, runtime: &Path) -> Result<Self> {
        let environment = super::runtime::environment(runtime)?;
        let cpu = Session::builder(&environment)?
            .with_intra_threads(1)
            .map_err(ort::Error::<()>::from)?
            .with_inter_threads(1)
            .map_err(ort::Error::<()>::from)?
            .commit_from_file(directory.join("model.onnx"))?;
        let mut tokenizer = Tokenizer::from_file(directory.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 7999,
                strategy: TruncationStrategy::OnlySecond,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(Self {
            cpu,
            cuda: super::runtime::cuda_selected(),
            environment,
            tokenizer,
        })
    }
    pub fn preferred_device(&self) -> Option<StudentDevice> {
        if super::runtime::directml_selected() {
            Some(StudentDevice::DirectMl)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(StudentDevice::CoreMl)
        } else if self.cuda {
            Some(StudentDevice::Cuda)
        } else {
            None
        }
    }
    pub fn load_gpu(&self, directory: &Path, device: StudentDevice) -> Result<Gpu> {
        if device == StudentDevice::CoreMl {
            return Ok(Gpu {
                device,
                session: Mutex::new(self.load_coreml(directory, true)?),
            });
        }
        let provider = match device {
            StudentDevice::DirectMl => ort::ep::DirectML::default().build().error_on_failure(),
            StudentDevice::Cuda => ort::ep::CUDA::default().build().error_on_failure(),
            _ => bail!("invalid GPU provider"),
        };
        let session = Session::builder(&self.environment)?
            .with_intra_threads(1)
            .map_err(ort::Error::<()>::from)?
            .with_inter_threads(1)
            .map_err(ort::Error::<()>::from)?
            .with_memory_pattern(false)
            .map_err(ort::Error::<()>::from)?
            .with_parallel_execution(false)
            .map_err(ort::Error::<()>::from)?
            .with_execution_providers([provider, ort::ep::CPU::default().build()])
            .map_err(ort::Error::<()>::from)?
            .with_profiling(profile_prefix(directory)?)
            .map_err(ort::Error::<()>::from)?
            .commit_from_file(directory.join("model.onnx"))?;
        Ok(Gpu {
            device,
            session: Mutex::new(GpuSessions::Dynamic(Bucket {
                length: 0,
                session,
                check_placement: Some(if device == StudentDevice::Cuda {
                    "CUDAExecutionProvider"
                } else {
                    "DmlExecutionProvider"
                }),
            })),
        })
    }
    pub(super) fn load_coreml(&self, directory: &Path, accelerated: bool) -> Result<GpuSessions> {
        #[derive(serde::Deserialize)]
        struct Initializer {
            name: String,
            shape: Vec<usize>,
            dtype: String,
            location: String,
            offset: usize,
            length: usize,
        }
        // Register actual fp16 values, not file-backed OrtValues: ORT1.23's
        // CoreML EP otherwise hits ENOTDIR while mapping external weights.
        let weights = std::fs::read(directory.join("weights.fp16.data"))?;
        let mut shared = BTreeMap::new();
        let mut buckets = Vec::new();
        for n in [6144, 8192] {
            let graph = format!("model-coreml-{n}.onnx");
            let pin = super::assets::Manifest::pinned()
                .files
                .iter()
                .find(|f| f.name == graph)
                .context("missing CoreML graph pin")?;
            let cache = directory.join("coreml").join(&pin.sha256);
            std::fs::create_dir_all(&cache)?;
            let mut builder = Session::builder(&self.environment)?
                .with_intra_threads(1)
                .map_err(ort::Error::<()>::from)?
                .with_inter_threads(1)
                .map_err(ort::Error::<()>::from)?
                .with_memory_pattern(false)
                .map_err(ort::Error::<()>::from)?
                .with_parallel_execution(false)
                .map_err(ort::Error::<()>::from)?;
            let initializers: Vec<Initializer> = serde_json::from_slice(&std::fs::read(
                directory.join(format!("initializers-{n}.json")),
            )?)?;
            for i in initializers {
                ensure!(
                    i.dtype == "float16" && i.location == "weights.fp16.data",
                    "unexpected external initializer type/location"
                );
                let key = (i.offset, i.length, i.shape.clone());
                let tensor = if let Some(tensor) = shared.get(&key) {
                    Arc::clone(tensor)
                } else {
                    let data = weights
                        .get(
                            i.offset
                                ..i.offset
                                    .checked_add(i.length)
                                    .context("initializer offset overflow")?,
                        )
                        .context("initializer outside pinned weights")?;
                    ensure!(data.len() % 2 == 0, "odd fp16 initializer length");
                    let values: Vec<half::f16> = data
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| half::f16::from_bits(u16::from_le_bytes([b[0], b[1]])))
                        .collect();
                    let tensor = Arc::new(Tensor::from_array((i.shape, values))?.into_dyn());
                    shared.insert(key, tensor.clone());
                    tensor
                };
                builder = builder
                    .with_external_initializer(&i.name, tensor)
                    .map_err(ort::Error::<()>::from)?;
            }
            if accelerated {
                builder = builder
                    .with_execution_providers([
                        ort::ep::CoreML::default()
                            .with_model_format(ort::ep::coreml::ModelFormat::MLProgram)
                            .with_compute_units(ort::ep::coreml::ComputeUnits::CPUAndGPU)
                            .with_static_input_shapes(true)
                            .with_low_precision_accumulation_on_gpu(false)
                            .with_model_cache_dir(cache.to_string_lossy())
                            .build()
                            .error_on_failure(),
                        ort::ep::CPU::default().build(),
                    ])
                    .map_err(ort::Error::<()>::from)?;
                builder = builder
                    .with_profiling(profile_prefix(&cache)?)
                    .map_err(ort::Error::<()>::from)?;
            }
            let session = builder.commit_from_file(directory.join(graph))?;
            buckets.push(Bucket {
                length: n,
                session,
                check_placement: accelerated.then_some("CoreMLExecutionProvider"),
            });
        }
        Ok(GpuSessions::CoreMl(buckets))
    }
    pub fn tokens(&self, item: &str, session: &str) -> Result<(Vec<i64>, Vec<i64>)> {
        let encoded = self
            .tokenizer
            .encode((item, session), true)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("tokenizing GitHub relevance pair")?;
        Ok((
            encoded.get_ids().iter().map(|&v| i64::from(v)).collect(),
            encoded
                .get_attention_mask()
                .iter()
                .map(|&v| i64::from(v))
                .collect(),
        ))
    }
    pub fn score_tokens(&self, ids: Vec<i64>, mask: Vec<i64>) -> Result<f64> {
        super::concurrent::score(&self.cpu, ids, mask)
    }
}
pub(super) struct Bucket {
    length: usize,
    session: Session,
    check_placement: Option<&'static str>,
}
pub(super) enum GpuSessions {
    Dynamic(Bucket),
    CoreMl(Vec<Bucket>),
}
impl GpuSessions {
    pub fn warmup(&mut self, ids: Vec<i64>, mask: Vec<i64>) -> Result<()> {
        self.score(ids, mask).map(|_| ())
    }
    pub fn score(&mut self, mut ids: Vec<i64>, mut mask: Vec<i64>) -> Result<f64> {
        let bucket = match self {
            Self::Dynamic(bucket) => bucket,
            Self::CoreMl(buckets) => {
                let bucket = buckets
                    .iter_mut()
                    .find(|b| ids.len() <= b.length)
                    .context("pair exceeds CoreML bucket")?;
                ids.resize(bucket.length, 0);
                mask.resize(bucket.length, 0);
                bucket
            }
        };
        let score = score_gpu(&mut bucket.session, ids, mask)?;
        if let Some(provider) = bucket.check_placement {
            let profile = PathBuf::from(bucket.session.end_profiling()?);
            let parsed = (|| -> Result<()> {
                let events: Vec<serde_json::Value> =
                    serde_json::from_slice(&std::fs::read(&profile)?)?;
                ensure!(
                    events.iter().any(|e| e["args"]["provider"] == provider),
                    "{provider} graph ran wholly on CPU"
                );
                Ok(())
            })();
            std::fs::remove_file(profile)?;
            parsed?;
            bucket.check_placement = None;
        }
        Ok(score)
    }
}

pub(super) fn score_gpu(session: &mut Session, ids: Vec<i64>, mask: Vec<i64>) -> Result<f64> {
    let n = ids.len();
    ensure!(mask.len() == n, "student mask length mismatch");
    let values = session.run(ort::inputs![
        "input_ids"=>Tensor::from_array(([1,n],ids))?,
        "attention_mask"=>Tensor::from_array(([1,n],mask))?
    ])?;
    let (_, logits) = values[0].try_extract_tensor::<f32>()?;
    ensure!(
        logits.len() == 1,
        "student returned {} logits",
        logits.len()
    );
    ensure!(logits[0].is_finite(), "student returned nonfinite logit");
    let probability = 1.0 / (1.0 + (-f64::from(logits[0])).exp());
    ensure!(
        probability.is_finite(),
        "student returned nonfinite probability"
    );
    Ok(probability)
}
pub(super) fn parallelism() -> Result<usize> {
    let cores = std::thread::available_parallelism()?.get();
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    // Measured ORT1.23.0 rank3 graph, fp32 CPU, one intra-op thread:
    // 5649-token p95 grew RSS by458452992B (lean/ort1230-rank3-p95.log).
    // The documented7999-token maximum measured599478272B growth; use
    // that larger observed peak so a maximum-length fan-out also fits.
    // Evidence: lean/max-context-memory.log, ORT1.23.0, arena enabled.
    const MEASURED_RUN_BYTES: u64 = 599_478_272;
    let memory_runs = usize::try_from(system.available_memory() / MEASURED_RUN_BYTES)?;
    ensure!(
        memory_runs > 0,
        "insufficient available memory for one measured student inference"
    );
    Ok(cores.min(memory_runs))
}
