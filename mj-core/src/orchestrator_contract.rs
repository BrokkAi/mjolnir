//! Shared orchestration contracts and report delivery state.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use tokio::sync::{mpsc, watch};

use crate::event::SubagentOutcome;

pub const PROGRESS_WAKE_INSTRUCTION: &str = "No subagent has finished yet. Decide: keep waiting (end your turn again), redirect or take over the work yourself, or cancel a subagent.";

#[derive(Clone, Debug)]
pub struct ActiveSubagentWorkers {
    updates: watch::Sender<usize>,
}

impl Default for ActiveSubagentWorkers {
    fn default() -> Self {
        let (updates, _) = watch::channel(0);
        Self { updates }
    }
}

impl ActiveSubagentWorkers {
    pub fn subscribe(&self) -> watch::Receiver<usize> {
        self.updates.subscribe()
    }

    pub fn set(&self, count: usize) {
        self.updates.send_replace(count);
    }
}

#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub subagent_id: u64,
    pub label: String,
    pub agent: String,
    pub model: String,
    pub outcome: SubagentOutcome,
    pub final_message: String,
    pub slim_activity: String,
    pub workspace_diff: Option<String>,
    pub debrief: Option<String>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct SubagentReportBus {
    tx: mpsc::UnboundedSender<SubagentReport>,
    accounting: Arc<Mutex<SubagentReportAccounting>>,
}

#[derive(Debug, Default)]
struct SubagentReportAccounting {
    pending: HashSet<u64>,
    claimed: HashSet<u64>,
}

impl SubagentReportBus {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<SubagentReport>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                accounting: Arc::default(),
            },
            rx,
        )
    }

    pub fn pending(&self) -> usize {
        self.lock_accounting().pending.len()
    }

    pub fn claim(&self, subagent_id: u64) {
        let mut accounting = self.lock_accounting();
        if accounting.claimed.insert(subagent_id) {
            accounting.pending.remove(&subagent_id);
        }
    }

    pub fn take_claim(&self, subagent_id: u64) -> bool {
        self.lock_accounting().claimed.remove(&subagent_id)
    }

    fn lock_accounting(&self) -> std::sync::MutexGuard<'_, SubagentReportAccounting> {
        self.accounting
            .lock()
            .expect("subagent report claim lock poisoned")
    }

    pub fn open(&self, subagent_id: u64) {
        self.lock_accounting().pending.insert(subagent_id);
    }

    pub fn deliver(&self, report: SubagentReport) {
        let subagent_id = report.subagent_id;
        if self.tx.send(report).is_err() {
            self.close(subagent_id);
        }
    }

    pub fn close(&self, subagent_id: u64) {
        self.lock_accounting().pending.remove(&subagent_id);
    }
}

pub fn format_report_injection(
    reports: &[SubagentReport],
    progress: Option<&str>,
    trailing_instruction: &str,
) -> String {
    let mut out = String::new();
    for report in reports {
        out.push_str(&format_report_block(report, true));
        out.push_str("\n\n");
    }
    if let Some(progress) = progress.map(str::trim).filter(|block| !block.is_empty()) {
        out.push_str(progress);
        out.push_str("\n\n");
    }
    out.push_str(trailing_instruction);
    out
}

pub fn format_report_block(report: &SubagentReport, session_note: bool) -> String {
    let diff = report
        .workspace_diff
        .as_deref()
        .unwrap_or("[workspace snapshot unavailable for this subagent]");
    let debrief = report
        .debrief
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| format!("<debrief>\n{text}\n</debrief>\n"))
        .unwrap_or_default();
    let session_note = match &report.outcome {
        SubagentOutcome::Completed if session_note => format!(
            "<session>\nThis subagent's session is retained with its full working context. For follow-up work that needs the same context, create_subagent with resume={id} continues it and is far cheaper than a new subagent loading that context from scratch. Work needing different context is better served by a fresh subagent. subagent_cancel with subagent_id {id} releases it.\n</session>\n",
            id = report.subagent_id
        ),
        _ => String::new(),
    };
    format!(
        "<subagent_result id=\"{id}\" label=\"{label}\" agent=\"{agent}\" model=\"{model}\" outcome=\"{outcome}\" elapsed=\"{elapsed}\">\n<report>\n{report_text}\n</report>\n{debrief}<activity_summary>\n{activity}\n</activity_summary>\n<workspace_diff>\n{diff}\n</workspace_diff>\n{session_note}</subagent_result>",
        id = report.subagent_id,
        label = escape_report_attribute(&report.label),
        agent = escape_report_attribute(&report.agent),
        model = escape_report_attribute(&report.model),
        outcome = report.outcome.label(),
        elapsed = format_report_elapsed(report.elapsed),
        report_text = report.final_message.trim(),
        activity = report.slim_activity.trim(),
    )
}

pub fn format_progress_wake(progress: &str) -> String {
    format!("{}\n\n{PROGRESS_WAKE_INSTRUCTION}", progress.trim())
}

fn escape_report_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace(['\n', '\r'], " ")
}

pub fn format_report_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_bus_tracks_and_claims_individual_reports() {
        let (bus, _reports) = SubagentReportBus::channel();
        bus.open(1);
        bus.open(2);
        bus.claim(1);
        assert_eq!(bus.pending(), 1);
        assert!(bus.take_claim(1));
        assert!(!bus.take_claim(1));
        bus.close(2);
        assert_eq!(bus.pending(), 0);
    }
}

/// Convert a configured heartbeat in minutes into an optional wake interval.
pub fn progress_wake_interval(minutes: u64) -> Option<Duration> {
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}

#[doc(hidden)]
pub async fn heartbeat_tick(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Agent-side service queried by the core orchestrator for live progress.
pub trait SubagentProgressSource: Send + Sync {
    fn progress_block(&self) -> BoxFuture<'_, Option<String>>;
}

#[derive(Clone)]
pub struct SubagentProgressService(Arc<dyn SubagentProgressSource>);

impl SubagentProgressService {
    pub fn new(service: impl SubagentProgressSource + 'static) -> Self {
        Self(Arc::new(service))
    }

    pub async fn progress_block(&self) -> Option<String> {
        self.0.progress_block().await
    }
}

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::ReviewTier;
use crate::event::{PromptImage, UiEvent};
use crate::workflow::{WorkflowEmitter, WorkflowId};
use crate::workspace_snapshot::ReviewSnapshot;

pub struct ReviewJob {
    pub epoch: u64,
    pub workflow_id: WorkflowId,
    pub review_pass: u32,
    pub tier: ReviewTier,
    pub workflow: WorkflowEmitter,
    pub task: String,
    pub images: Vec<PromptImage>,
    pub user_messages: Vec<String>,
    pub initial_result: String,
    pub trajectory: String,
    pub diff: String,
    pub snapshot: Option<ReviewSnapshot>,
    pub focus_snapshot: Option<ReviewSnapshot>,
    pub prior_review: Option<PriorReviewContext>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewPassEvidence {
    pub intent_brief: String,
    pub intent_available: bool,
    pub lanes: Vec<ReviewLaneEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLaneEvidence {
    pub id: String,
    pub outcome: SubagentOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorReviewContext {
    pub synthesis: String,
    pub evidence: ReviewPassEvidence,
    pub exact_delta: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    Findings {
        synthesis: String,
        evidence: ReviewPassEvidence,
    },
    Advisory {
        synthesis: String,
        evidence: ReviewPassEvidence,
    },
    Clean,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub epoch: u64,
    pub verdict: ReviewVerdict,
}

type ReviewSpawnFn = dyn Fn(
        ReviewJob,
        mpsc::UnboundedSender<UiEvent>,
        CancellationToken,
        mpsc::UnboundedSender<ReviewOutcome>,
    ) -> JoinHandle<()>
    + Send
    + Sync;

#[derive(Clone)]
pub struct ReviewSpawner(Arc<ReviewSpawnFn>);

impl std::fmt::Debug for ReviewSpawner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Spawner")
    }
}

impl ReviewSpawner {
    pub fn new(
        dispatch: impl Fn(
            ReviewJob,
            mpsc::UnboundedSender<UiEvent>,
            CancellationToken,
            mpsc::UnboundedSender<ReviewOutcome>,
        ) -> JoinHandle<()>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self(Arc::new(dispatch))
    }

    #[doc(hidden)]
    pub fn stub(
        dispatch: impl Fn(
            ReviewJob,
            mpsc::UnboundedSender<UiEvent>,
            CancellationToken,
            mpsc::UnboundedSender<ReviewOutcome>,
        ) + Send
        + Sync
        + 'static,
    ) -> Self {
        let dispatch = Arc::new(dispatch);
        Self::new(move |job, events, cancel, outcomes| {
            let dispatch = Arc::clone(&dispatch);
            tokio::spawn(async move { dispatch(job, events, cancel, outcomes) })
        })
    }

    #[doc(hidden)]
    pub fn stub_async<F, Fut>(dispatch: F) -> Self
    where
        F: Fn(
                ReviewJob,
                mpsc::UnboundedSender<UiEvent>,
                CancellationToken,
                mpsc::UnboundedSender<ReviewOutcome>,
            ) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let dispatch = Arc::new(dispatch);
        Self::new(move |job, events, cancel, outcomes| {
            tokio::spawn(dispatch(job, events, cancel, outcomes))
        })
    }

    pub fn spawn(
        &self,
        job: ReviewJob,
        events: mpsc::UnboundedSender<UiEvent>,
        cancel: CancellationToken,
        outcomes: mpsc::UnboundedSender<ReviewOutcome>,
    ) -> JoinHandle<()> {
        (self.0)(job, events, cancel, outcomes)
    }
}

pub fn review_section_limits(trajectory_len: usize, diff_len: usize) -> (usize, usize) {
    const TOTAL: usize = 128 * 1024;
    const TRAJECTORY_SHARE: usize = 32 * 1024;
    let mut trajectory = trajectory_len.min(TRAJECTORY_SHARE);
    let mut diff = diff_len.min(TOTAL - TRAJECTORY_SHARE);
    let mut remaining = TOTAL.saturating_sub(trajectory + diff);
    let diff_extra = diff_len.saturating_sub(diff).min(remaining);
    diff += diff_extra;
    remaining -= diff_extra;
    trajectory += trajectory_len.saturating_sub(trajectory).min(remaining);
    (trajectory, diff)
}

pub fn bound_review_section(text: &str, limit: usize, label: &str) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let marker = format!("\n…[{label} omitted]…\n");
    let available = limit.saturating_sub(marker.len());
    let head = available.saturating_mul(3) / 4;
    let tail = available.saturating_sub(head);
    let head_end = text.floor_char_boundary(head);
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail));
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}
