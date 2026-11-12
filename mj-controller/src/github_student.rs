//! One daemon-owned model state machine. Downloads, retry scheduling and device
//! transitions are decided here; readers never inspect the cache for readiness.
mod assets;
mod concurrent;
mod inference;
mod runtime;

use anyhow::{Context, Result};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StudentFailureKind {
    Download,
    Verification,
    Load,
    Inference,
    WindowsRuntimeMissing,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StudentState {
    Loading,
    Ready,
    Failed {
        kind: StudentFailureKind,
        cause: String,
        next_retry_at_ms: i64,
    },
}
/// Policy routes on this downcastable error, never on diagnostic text.
#[derive(Debug)]
pub enum StudentError {
    Unavailable(StudentState),
    Inference { cause: String },
}
impl std::fmt::Display for StudentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(state) => write!(f, "GitHub student unavailable: {state:?}"),
            Self::Inference { cause } => write!(f, "GitHub student inference failed: {cause}"),
        }
    }
}
impl std::error::Error for StudentError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StudentDevice {
    Cpu,
    DirectMl,
    CoreMl,
    Cuda,
}
#[derive(Clone, Debug)]
pub enum StudentDeviceFallbackReason {
    Slower { gpu_ms: u64, cpu_ms: u64 },
    Failed(StudentFailureKind),
}
#[derive(Clone, Debug)]
pub enum StudentNotice {
    DeviceFallback {
        from: StudentDevice,
        reason: StudentDeviceFallbackReason,
    },
    PlatformUnsupported,
    GpuAfterRestart,
    WindowsRuntimeMissing {
        missing: Vec<String>,
    },
    CudaLibrariesMissing {
        missing: Vec<String>,
    },
    InferenceFailing {
        cause: String,
    },
    InferenceRecovered,
}
type Notice = Arc<dyn Fn(StudentNotice) + Send + Sync>;

enum Device {
    Cpu,
    Needed,
    Preparing(Arc<CancellationToken>),
    Pending(Arc<inference::Gpu>),
    Benchmarking(Arc<inference::Gpu>),
    Gpu(Arc<inference::Gpu>),
}
struct Ready {
    engine: Arc<inference::Engine>,
    permits: Arc<Semaphore>,
    device: Device,
    failing: bool,
}
struct Failure {
    kind: StudentFailureKind,
    cause: String,
    retry_at: Instant,
    retry_at_ms: i64,
}
enum State {
    Dormant,
    Loading,
    Ready(Ready),
    Failed(Failure),
}
impl State {
    fn public(&self) -> StudentState {
        match self {
            Self::Dormant | Self::Loading => StudentState::Loading,
            Self::Ready(_) => StudentState::Ready,
            Self::Failed(f) => StudentState::Failed {
                kind: f.kind,
                cause: f.cause.clone(),
                next_retry_at_ms: f.retry_at_ms,
            },
        }
    }
}
struct Machine {
    state: State,
    failures: u32,
    use_gpu: bool,
}
struct Inner {
    #[cfg(test)]
    probe_control_failures: std::sync::atomic::AtomicUsize,
    machine: Mutex<Machine>,
    changed: Notify,
    notice: Notice,
}
impl Inner {
    fn fail(machine: &mut Machine, kind: StudentFailureKind, error: &anyhow::Error) {
        machine.failures = machine.failures.saturating_add(1);
        let detail = error.downcast_ref::<PreparationFailure>();
        let kind = detail.map_or(kind, |e| e.kind);
        // Same retry progression as the existing GitHub credential lookup:
        // one second, doubling up to its eighth exponent, with server hints.
        let delay = Duration::from_secs(1_u64 << machine.failures.saturating_sub(1).min(8));
        let delay = detail
            .and_then(|e| e.retry_after)
            .map_or(delay, |hint| delay.max(hint));
        tracing::warn!(error=%format!("{error:#}"),?kind,retry_seconds=delay.as_secs(),"GitHub student failed; automatic retry scheduled");
        machine.state = State::Failed(Failure {
            kind,
            cause: format!("{error:#}"),
            retry_at: Instant::now() + delay,
            retry_at_ms: chrono::Utc::now().timestamp_millis() + delay.as_millis() as i64,
        });
    }
    fn completed(&self, engine: &Arc<inference::Engine>, result: &Result<f64>) {
        let mut machine = self.machine.lock().expect("student state poisoned");
        // A late result from a replaced engine cannot change the new epoch.
        if !matches!(&machine.state, State::Ready(ready) if Arc::ptr_eq(&ready.engine,engine)) {
            return;
        }
        let State::Ready(ready) = &mut machine.state else {
            return;
        };
        let notice = match result {
            Err(error) if !ready.failing => {
                ready.failing = true;
                tracing::warn!(error=%format!("{error:#}"), "GitHub student inference failing");
                Some(StudentNotice::InferenceFailing {
                    cause: format!("{error:#}"),
                })
            }
            Ok(_) if ready.failing => {
                ready.failing = false;
                Some(StudentNotice::InferenceRecovered)
            }
            _ => None,
        };
        drop(machine);
        if let Some(notice) = notice {
            (self.notice)(notice);
        }
    }
}
struct PreparationOptions {
    model_base: String,
    model_manifest: assets::Manifest,
    #[cfg(test)]
    load_failures: Option<Arc<std::sync::atomic::AtomicUsize>>,
}
impl Default for PreparationOptions {
    fn default() -> Self {
        Self {
            model_base: "https://huggingface.co".into(),
            model_manifest: assets::Manifest::pinned().clone(),
            #[cfg(test)]
            load_failures: None,
        }
    }
}
/// Keep one handle per daemon. The callback receives typed transitions and is
/// called on background work; public wording belongs to the notice consumer.
pub struct Student {
    inner: Arc<Inner>,
    cancellation: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    runtime: tokio::runtime::Handle,
}
impl Student {
    pub fn new(data_directory: PathBuf, use_gpu: bool, notice: Notice) -> Self {
        Self::start(
            data_directory,
            use_gpu,
            notice,
            PreparationOptions::default(),
        )
    }
    fn start(
        data_directory: PathBuf,
        use_gpu: bool,
        notice: Notice,
        options: PreparationOptions,
    ) -> Self {
        let inner = Arc::new(Inner {
            #[cfg(test)]
            probe_control_failures: Default::default(),
            machine: Mutex::new(Machine {
                state: State::Dormant,
                use_gpu,
                failures: 0,
            }),
            changed: Notify::new(),
            notice,
        });
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(supervise(
            inner.clone(),
            data_directory,
            cancellation.clone(),
            options,
        ));
        Self {
            inner,
            cancellation,
            task: Mutex::new(Some(task)),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    /// A preference change starts one fresh probe. Repeated configuration
    /// reloads preserve a measured fallback; disabling retains the CPU engine.
    pub fn set_use_gpu(&self, enabled: bool) {
        let old = {
            let mut machine = self.inner.machine.lock().expect("student state poisoned");
            if machine.use_gpu == enabled {
                return;
            }
            machine.use_gpu = enabled;
            if let State::Ready(ready) = &mut machine.state {
                Some(std::mem::replace(
                    &mut ready.device,
                    if enabled { Device::Needed } else { Device::Cpu },
                ))
            } else {
                None
            }
        };
        if let Some(Device::Preparing(request)) = &old {
            request.cancel();
        }
        drop(self.runtime.spawn_blocking(move || drop(old)));
        if enabled && runtime::gpu_requires_restart() {
            (self.inner.notice)(StudentNotice::GpuAfterRestart);
        }
        self.inner.changed.notify_one();
    }
    pub fn state(&self) -> StudentState {
        self.inner
            .machine
            .lock()
            .expect("student state poisoned")
            .state
            .public()
    }

    pub fn manifest_revision(&self) -> &str {
        &assets::Manifest::pinned().revision
    }
    pub fn score_segments<'a>(
        &'a self,
        item: &'a str,
        session: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<f64>> + Send + 'a>> {
        Box::pin(async move {
            {
                let mut machine = self.inner.machine.lock().expect("student state poisoned");
                if matches!(machine.state, State::Dormant) {
                    machine.state = State::Loading;
                    self.inner.changed.notify_one();
                    return Err(StudentError::Unavailable(StudentState::Loading).into());
                }
            }
            // Queue time is outside every measured inference duration. Reserve
            // the actual device only after admission, using the owner's state.
            let (permits, waiting_epoch) = {
                let machine = self.inner.machine.lock().expect("student state poisoned");
                match &machine.state {
                    State::Ready(ready) => (ready.permits.clone(), Arc::downgrade(&ready.engine)),
                    _ => return Err(StudentError::Unavailable(machine.state.public()).into()),
                }
            };
            let permit = tokio::select! {
                _=self.cancellation.cancelled()=>return Err(StudentError::Unavailable(StudentState::Loading).into()),
                permit=permits.acquire_owned()=>permit?,
            };
            let (engine, work) = {
                let mut machine = self.inner.machine.lock().expect("student state poisoned");
                let State::Ready(ready) = &mut machine.state else {
                    return Err(StudentError::Unavailable(machine.state.public()).into());
                };
                if !std::sync::Weak::ptr_eq(&waiting_epoch, &Arc::downgrade(&ready.engine)) {
                    return Err(StudentError::Unavailable(machine.state.public()).into());
                }
                let work = match &ready.device {
                    Device::Pending(gpu) => {
                        let gpu = gpu.clone();
                        ready.device = Device::Benchmarking(gpu.clone());
                        Work::Benchmark(Probe {
                            inner: self.inner.clone(),
                            engine: ready.engine.clone(),
                            gpu,
                            outcome: ProbeOutcome::ControlFailed,
                        })
                    }
                    Device::Gpu(gpu) => Work::Gpu(gpu.clone()),
                    Device::Cpu
                    | Device::Needed
                    | Device::Preparing(_)
                    | Device::Benchmarking(_) => Work::Cpu,
                };
                (ready.engine.clone(), work)
            };
            let item = item.to_owned();
            let session = session.to_owned();
            let inner = self.inner.clone();
            let epoch = Arc::downgrade(&engine);
            let task = tokio::task::spawn_blocking(move || {
                // Fields drop in declaration order, releasing model memory
                // before returning the admission permit, including on unwind.
                struct Active {
                    engine: Arc<inference::Engine>,
                    _permit: tokio::sync::OwnedSemaphorePermit,
                }
                let active = Active {
                    engine,
                    _permit: permit,
                };
                let result = run(&inner, &active.engine, work, &item, &session);
                inner.completed(&active.engine, &result);
                result
            });
            tokio::select! {
                _=self.cancellation.cancelled()=>Err(StudentError::Unavailable(StudentState::Loading).into()),
                outcome=task=>match outcome {
                    Ok(result)=>result.map_err(|error|StudentError::Inference{cause:format!("{error:#}")}.into()),
                    Err(error)=>{
                        let result=Err(anyhow::Error::new(error).context("GitHub student inference task failed"));
                        if let Some(epoch)=epoch.upgrade(){self.inner.completed(&epoch,&result);}result.map_err(|error|StudentError::Inference{cause:format!("{error:#}")}.into())
                    }
                }
            }
        })
    }
    /// Cancels preparation promptly; in-flight inference owns no daemon handoff
    /// admission and its supervised blocking closure reports its own outcome.
    pub async fn shutdown(&self) -> Result<()> {
        self.cancellation.cancel();
        let task = self.task.lock().expect("student task poisoned").take();
        if let Some(task) = task {
            task.await.context("student supervisor failed")?;
        }
        self.release_state();
        Ok(())
    }
    fn release_state(&self) {
        let old = {
            let mut machine = self.inner.machine.lock().expect("student state poisoned");
            std::mem::replace(&mut machine.state, State::Loading)
        };
        // Native graph/session destruction can release hundreds of MB. It
        // belongs to background work even if the handle is dropped by a UI.
        drop(self.runtime.spawn_blocking(move || drop(old)));
    }
}
impl Drop for Student {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.release_state();
    }
}

enum Work {
    Cpu,
    Benchmark(Probe),
    Gpu(Arc<inference::Gpu>),
}

/// A control failure makes no device decision. Pending is consumed only by a
/// later real pair; neither the supervisor nor this probe retries it in a loop.
enum ProbeOutcome {
    GpuWins,
    CpuWins { gpu_ms: u64, cpu_ms: u64 },
    GpuFailed,
    ControlFailed,
}

/// Own the reservation from admission through blocking execution, including
/// cancellation before execution and unwinding. Exactly one drop resolves it.
struct Probe {
    inner: Arc<Inner>,
    engine: Arc<inference::Engine>,
    gpu: Arc<inference::Gpu>,
    outcome: ProbeOutcome,
}
impl Probe {
    fn complete(mut self, outcome: ProbeOutcome) {
        self.outcome = outcome;
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        device_transition(
            &self.inner,
            &self.engine,
            &self.gpu,
            std::mem::replace(&mut self.outcome, ProbeOutcome::ControlFailed),
        );
    }
}

fn device_transition(
    inner: &Inner,
    engine: &Arc<inference::Engine>,
    gpu: &Arc<inference::Gpu>,
    outcome: ProbeOutcome,
) {
    // Reservation cleanup must work even while another failure is unwinding.
    let mut machine = inner.machine.lock().unwrap_or_else(|p| p.into_inner());
    if let State::Ready(ready) = &mut machine.state
        && Arc::ptr_eq(&ready.engine, engine)
        && matches!(&ready.device,Device::Gpu(current)|Device::Benchmarking(current) if Arc::ptr_eq(current,gpu))
    {
        let (device, reason) = match outcome {
            ProbeOutcome::GpuWins => (Device::Gpu(gpu.clone()), None),
            ProbeOutcome::ControlFailed => (Device::Pending(gpu.clone()), None),
            ProbeOutcome::CpuWins { gpu_ms, cpu_ms } => (
                Device::Cpu,
                Some(StudentDeviceFallbackReason::Slower { gpu_ms, cpu_ms }),
            ),
            ProbeOutcome::GpuFailed => (
                Device::Cpu,
                Some(StudentDeviceFallbackReason::Failed(
                    StudentFailureKind::Inference,
                )),
            ),
        };
        ready.device = device;
        drop(machine);
        if let Some(reason) = reason {
            (inner.notice)(StudentNotice::DeviceFallback {
                from: gpu.device,
                reason,
            });
        }
    }
}

fn run(
    inner: &Inner,
    engine: &Arc<inference::Engine>,
    work: Work,
    item: &str,
    session: &str,
) -> Result<f64> {
    match work {
        Work::Cpu => {
            let (ids, mask) = engine.tokens(item, session)?;
            engine.score_tokens(ids, mask)
        }
        Work::Benchmark(probe) => {
            let (outcome, result) = probe_pair(inner, engine, &probe.gpu, item, session);
            probe.complete(outcome);
            result
        }
        Work::Gpu(gpu) => {
            let (ids, mask) = engine.tokens(item, session)?;
            let result = (|| -> Result<Option<f64>> {
                // DirectML permits one Run per session. Check selection after
                // that admission so queued calls cannot undo CPU selection.
                let mut session = gpu
                    .session
                    .lock()
                    .map_err(|_| anyhow::anyhow!("GPU session poisoned"))?;
                let machine = inner.machine.lock().expect("student state poisoned");
                let selected = matches!(&machine.state, State::Ready(ready)
                    if Arc::ptr_eq(&ready.engine, engine)
                    && matches!(&ready.device, Device::Gpu(current) if Arc::ptr_eq(current, &gpu)));
                drop(machine);
                if !selected {
                    return Ok(None);
                }
                session.score(ids.clone(), mask.clone()).map(Some)
            })();
            match result {
                Ok(Some(score)) => Ok(score),
                Ok(None) => engine.score_tokens(ids, mask),
                Err(error) => {
                    tracing::warn!(%error, device=?gpu.device, "GPU student inference failed; retaining CPU");
                    device_transition(inner, engine, &gpu, ProbeOutcome::GpuFailed);
                    engine.score_tokens(ids, mask)
                }
            }
        }
    }
}

/// Produce one outcome on every ordinary exit; the reservation guard supplies
/// ControlFailed if this computation unwinds instead of returning an outcome.
fn probe_pair(
    inner: &Inner,
    engine: &Arc<inference::Engine>,
    gpu: &Arc<inference::Gpu>,
    item: &str,
    session: &str,
) -> (ProbeOutcome, Result<f64>) {
    let (ids, mask) = match engine.tokens(item, session) {
        Ok(tokens) => tokens,
        Err(error) => return (ProbeOutcome::ControlFailed, Err(error)),
    };
    let start = std::time::Instant::now();
    #[cfg(test)]
    if inner
        .probe_control_failures
        .try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| n.checked_sub(1),
        )
        .is_ok()
    {
        return (
            ProbeOutcome::ControlFailed,
            Err(anyhow::anyhow!("recorded CPU-control inference failure")),
        );
    }
    let control = engine.score_tokens(ids.clone(), mask.clone());
    let cpu = match control {
        Ok(score) => score,
        Err(error) => return (ProbeOutcome::ControlFailed, Err(error)),
    };
    let cpu_time = start.elapsed();
    let mut session = match gpu.session.lock() {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, "GPU session poisoned; retaining CPU");
            return (ProbeOutcome::GpuFailed, Ok(cpu));
        }
    };
    let machine = inner.machine.lock().expect("student state poisoned");
    let selected = matches!(&machine.state, State::Ready(ready)
        if Arc::ptr_eq(&ready.engine, engine)
        && matches!(&ready.device, Device::Benchmarking(current) if Arc::ptr_eq(current, gpu)));
    drop(machine);
    // A preference or engine change has already released the reservation. Its
    // epoch check also prevents this stale outcome from changing that choice.
    if !selected {
        return (ProbeOutcome::ControlFailed, Ok(cpu));
    }
    // Discard cold GPU compilation/allocation before comparing warm latency.
    if let Err(error) = session.warmup(ids.clone(), mask.clone()) {
        tracing::warn!(%error, "GPU warmup failed; retaining CPU");
        return (ProbeOutcome::GpuFailed, Ok(cpu));
    }
    let start = std::time::Instant::now();
    let score = session.score(ids, mask);
    let gpu_time = start.elapsed();
    match score {
        Ok(score) if (cpu - score).abs() > 0.005 => {
            tracing::warn!(cpu,score,device=?gpu.device,"GPU student parity failed; retaining CPU");
            (ProbeOutcome::GpuFailed, Ok(cpu))
        }
        Ok(_) if gpu_time >= cpu_time => (
            ProbeOutcome::CpuWins {
                gpu_ms: gpu_time.as_millis() as u64,
                cpu_ms: cpu_time.as_millis() as u64,
            },
            Ok(cpu),
        ),
        Ok(score) => (ProbeOutcome::GpuWins, Ok(score)),
        Err(error) => {
            tracing::warn!(%error,device=?gpu.device,"GPU student inference failed; retaining CPU");
            (ProbeOutcome::GpuFailed, Ok(cpu))
        }
    }
}

async fn supervise(
    inner: Arc<Inner>,
    root: PathBuf,
    cancellation: CancellationToken,
    options: PreparationOptions,
) {
    loop {
        enum Action {
            Prepare(bool),
            Gpu(Arc<inference::Engine>, Arc<CancellationToken>),
            Retry(Instant),
            Wait,
        }
        // Register before reading state so an off/on transition cannot be lost
        // between the decision and waiting for its notification.
        let changed = inner.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let action = {
            let mut machine = inner.machine.lock().expect("student state poisoned");
            let enabled = machine.use_gpu;
            match &mut machine.state {
                State::Dormant => Action::Wait,
                State::Loading => Action::Prepare(enabled),
                State::Failed(f) => Action::Retry(f.retry_at),
                State::Ready(ready) if matches!(ready.device, Device::Needed) => {
                    let request = Arc::new(CancellationToken::new());
                    ready.device = Device::Preparing(request.clone());
                    Action::Gpu(ready.engine.clone(), request)
                }
                State::Ready(_) => Action::Wait,
            }
        };
        match action {
            Action::Wait => tokio::select! {_=cancellation.cancelled()=>return,_=&mut changed=>{}},
            Action::Retry(at) => {
                tokio::select! {_=cancellation.cancelled()=>return,_=tokio::time::sleep_until(at)=>{}}
                let mut machine = inner.machine.lock().expect("student state poisoned");
                if matches!(&machine.state,State::Failed(f) if f.retry_at==at) {
                    machine.state = State::Loading;
                    inner.changed.notify_waiters();
                }
            }
            Action::Gpu(engine, request) => {
                let preparation = async {
                    let client = reqwest::Client::builder().build().map_err(|e| {
                        PreparationFailure::error(StudentFailureKind::Download, e, None)
                    })?;
                    let device = engine.preferred_device();
                    let Some(device) = device else {
                        return Ok(None);
                    };
                    let directory = assets::prepare(
                        root.join("github-student"),
                        options
                            .model_manifest
                            .selected(device == StudentDevice::CoreMl),
                        &options.model_base,
                        &client,
                        &cancellation,
                    )
                    .await?;
                    let loaded = engine.clone();
                    let gpu =
                        tokio::task::spawn_blocking(move || loaded.load_gpu(&directory, device))
                            .await??;
                    Ok::<_, anyhow::Error>(Some(Arc::new(gpu)))
                };
                let result = tokio::select! {_=cancellation.cancelled()=>return,_=request.cancelled()=>continue,result=preparation=>result};
                let mut machine = inner.machine.lock().expect("student state poisoned");
                if let State::Ready(ready) = &mut machine.state
                    && Arc::ptr_eq(&ready.engine, &engine)
                    && matches!(&ready.device,Device::Preparing(current) if Arc::ptr_eq(current,&request))
                {
                    let notice = match &result {
                        Err(error) => {
                            tracing::warn!(%error,"GPU preparation failed; retaining CPU");
                            engine
                                .preferred_device()
                                .map(|from| StudentNotice::DeviceFallback {
                                    from,
                                    reason: StudentDeviceFallbackReason::Failed(
                                        StudentFailureKind::Load,
                                    ),
                                })
                        }
                        _ => None,
                    };
                    ready.device = match result {
                        Ok(Some(gpu)) => Device::Pending(gpu),
                        _ => Device::Cpu,
                    };
                    drop(machine);
                    if let Some(notice) = notice {
                        (inner.notice)(notice);
                    }
                } else {
                    drop(machine);
                    drop(tokio::task::spawn_blocking(move || drop(result)));
                }
            }
            Action::Prepare(use_gpu) => {
                let preparation = async {
                    // Decide unsupported hosts before starting any model bytes.
                    runtime::validate_platform()?;
                    let client = reqwest::Client::builder().build().map_err(|e| {
                        PreparationFailure::error(StudentFailureKind::Download, e, None)
                    })?;
                    let (directory, runtime) = tokio::try_join!(
                        assets::prepare(
                            root.join("github-student"),
                            options.model_manifest.selected(false),
                            &options.model_base,
                            &client,
                            &cancellation
                        ),
                        runtime::prepare_selected(
                            root.join("github-student-native"),
                            use_gpu,
                            &inner.notice,
                            &client,
                            &cancellation
                        )
                    )
                    .map_err(|e| {
                        if e.downcast_ref::<PreparationFailure>().is_some() {
                            e
                        } else {
                            PreparationFailure::error(StudentFailureKind::Download, e, None)
                        }
                    })?;
                    #[cfg(test)]
                    let load_failures = options.load_failures.clone();
                    tokio::task::spawn_blocking(move || -> Result<Ready> {
                        #[cfg(test)]
                        if let Some(failures) = load_failures
                            && failures
                                .try_update(
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                    |n| n.checked_sub(1),
                                )
                                .is_ok()
                        {
                            anyhow::bail!("recorded transient loader failure");
                        }
                        let engine = Arc::new(inference::Engine::load(&directory, &runtime)?);
                        let capacity = inference::parallelism()?;
                        Ok(Ready {
                            engine,
                            permits: Arc::new(Semaphore::new(capacity)),
                            device: Device::Cpu,
                            failing: false,
                        })
                    })
                    .await
                    .context("student loading task failed")?
                };
                let outcome =
                    tokio::select! {_=cancellation.cancelled()=>return,result=preparation=>result};
                let mut machine = inner.machine.lock().expect("student state poisoned");
                if cancellation.is_cancelled() {
                    drop(machine);
                    drop(tokio::task::spawn_blocking(move || drop(outcome)));
                    return;
                }
                let mut notice = None;
                match outcome {
                    Ok(mut ready) => {
                        ready.device = if machine.use_gpu {
                            Device::Needed
                        } else {
                            Device::Cpu
                        };
                        machine.failures = 0;
                        machine.state = State::Ready(ready);
                    }
                    Err(error) => {
                        notice = error
                            .downcast_ref::<PreparationFailure>()
                            .and_then(|e| e.notice.clone());
                        Inner::fail(&mut machine, StudentFailureKind::Load, &error);
                    }
                }
                drop(machine);
                inner.changed.notify_waiters();
                if let Some(notice) = notice {
                    (inner.notice)(notice);
                }
            }
        }
    }
}

#[derive(Debug)]
struct PreparationFailure {
    kind: StudentFailureKind,
    cause: String,
    retry_after: Option<Duration>,
    notice: Option<StudentNotice>,
}
impl PreparationFailure {
    fn error(
        kind: StudentFailureKind,
        error: impl std::fmt::Display,
        retry_after: Option<Duration>,
    ) -> anyhow::Error {
        Self {
            kind,
            cause: error.to_string(),
            retry_after,
            notice: None,
        }
        .into()
    }
}
impl std::fmt::Display for PreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)
    }
}
impl std::error::Error for PreparationFailure {}

#[cfg(test)]
mod tests;
