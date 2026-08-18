//! Proactive quota gating for background agent pools.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, mpsc};

use mj_core::claude_usage::{ClaudeUsageReport, ClaudeUsageStatus};
use mj_core::codex_usage::{CodexUsageClient, CodexUsageReport, CodexUsageStatus};
use mj_core::event::UiEvent;
use mj_core::roster::{AdapterKind, ResolvedAgent};

const CACHE_TTL: Duration = Duration::from_secs(60);
const REMAINING_LIMIT_PERCENT: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Clear,
    NearLimit { resets_at: Option<i64> },
    Unavailable,
}

struct Cached {
    checked_at: Instant,
    result: Check,
}

#[derive(Clone)]
pub struct Gate {
    cwd: PathBuf,
    cache: Arc<AsyncMutex<HashMap<String, Cached>>>,
    probe_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    codex: Arc<AsyncMutex<Option<CodexUsageClient>>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
}

impl Gate {
    pub fn new(cwd: PathBuf, ui_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            cwd,
            cache: Arc::default(),
            probe_locks: Arc::default(),
            codex: Arc::default(),
            ui_tx,
        }
    }

    pub async fn check(&self, role: &ResolvedAgent) -> Check {
        self.check_inner(role, false).await
    }

    async fn refresh(&self, role: &ResolvedAgent) -> Check {
        self.check_inner(role, true).await
    }

    async fn check_inner(&self, role: &ResolvedAgent, force: bool) -> Check {
        let key = role.launch.source_id.clone();
        if !force {
            let cached = self.cache.lock().await.get(&key).and_then(|cached| {
                (cached.checked_at.elapsed() <= CACHE_TTL).then(|| cached.result.clone())
            });
            if let Some(cached) = cached {
                return cached;
            }
        }
        let probe_lock = self
            .probe_locks
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .clone();
        let _probe = probe_lock.lock().await;
        if !force {
            let cached = self.cache.lock().await.get(&key).and_then(|cached| {
                (cached.checked_at.elapsed() <= CACHE_TTL).then(|| cached.result.clone())
            });
            if let Some(cached) = cached {
                return cached;
            }
        }

        let result = match role.launch.kind {
            AdapterKind::Claude => {
                // A forced recheck (after an agent failure) must not be
                // satisfied by a minute-old shared fact.
                let queried = if force {
                    mj_core::claude_usage::query_fresh(self.cwd.clone(), role.launch.env.clone())
                        .await
                } else {
                    mj_core::claude_usage::query(self.cwd.clone(), role.launch.env.clone()).await
                };
                match queried {
                    Ok(report) => {
                        let result = claude_check(&report);
                        let _ = self
                            .ui_tx
                            .send(UiEvent::ClaudeUsage(ClaudeUsageStatus::Available(report)));
                        result
                    }
                    Err(error) => {
                        let _ =
                            self.ui_tx
                                .send(UiEvent::ClaudeUsage(ClaudeUsageStatus::Unavailable(
                                    error.user_reason().to_string(),
                                )));
                        Check::Unavailable
                    }
                }
            }
            AdapterKind::Codex => {
                let mut client = self.codex.lock().await;
                match mj_core::codex_usage::refresh(
                    &mut client,
                    self.cwd.clone(),
                    role.launch.env.clone(),
                )
                .await
                {
                    CodexUsageStatus::Available(report) => {
                        let result = codex_check(&report);
                        let _ = self
                            .ui_tx
                            .send(UiEvent::CodexUsage(CodexUsageStatus::Available(report)));
                        result
                    }
                    CodexUsageStatus::Unavailable(reason) => {
                        let _ = self
                            .ui_tx
                            .send(UiEvent::CodexUsage(CodexUsageStatus::Unavailable(reason)));
                        Check::Unavailable
                    }
                }
            }
        };
        self.cache.lock().await.insert(
            key,
            Cached {
                checked_at: Instant::now(),
                result: result.clone(),
            },
        );
        result
    }
}

fn claude_check(report: &ClaudeUsageReport) -> Check {
    let near = [&report.five_hour, &report.week]
        .into_iter()
        .flatten()
        .any(|window| window.remaining_percent <= REMAINING_LIMIT_PERCENT);
    if near {
        Check::NearLimit { resets_at: None }
    } else {
        Check::Clear
    }
}

fn codex_check(report: &CodexUsageReport) -> Check {
    let windows = [&report.primary, &report.secondary]
        .into_iter()
        .flatten()
        .filter(|window| window.remaining_percent <= REMAINING_LIMIT_PERCENT)
        .collect::<Vec<_>>();
    if windows.is_empty() {
        Check::Clear
    } else {
        Check::NearLimit {
            resets_at: windows.iter().filter_map(|window| window.resets_at).min(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub role: ResolvedAgent,
}

#[derive(Clone)]
pub struct RolePool {
    roles: Arc<Vec<ResolvedAgent>>,
    state: Arc<Mutex<PoolState>>,
    gate: Gate,
    auto_failover: bool,
    /// Human-readable name of the pool, used in quota status messages.
    label: &'static str,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
}

#[derive(Default)]
struct PoolState {
    current: usize,
    excluded_providers: HashSet<String>,
    announced_block: bool,
}

impl RolePool {
    pub fn new(
        roles: Vec<ResolvedAgent>,
        gate: Gate,
        auto_failover: bool,
        label: &'static str,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        assert!(!roles.is_empty(), "role pool requires an initial role");
        Self {
            roles: Arc::new(roles),
            state: Arc::default(),
            gate,
            auto_failover,
            label,
            ui_tx,
        }
    }

    pub fn current(&self) -> ResolvedAgent {
        let state = self.state.lock().expect("role pool poisoned");
        self.roles[state.current].clone()
    }

    pub async fn select_for_work(&self) -> Result<Selection, String> {
        loop {
            let role = {
                let state = self.state.lock().expect("role pool poisoned");
                self.roles[state.current].clone()
            };
            match self.gate.check(&role).await {
                Check::Clear | Check::Unavailable => {
                    self.state
                        .lock()
                        .expect("role pool poisoned")
                        .announced_block = false;
                    return Ok(Selection { role });
                }
                Check::NearLimit { resets_at } => {
                    if self.handle_near_limit(&role, resets_at) {
                        continue;
                    }
                    return Err(format!(
                        "{} is paused because {} quota has 5% or less remaining",
                        self.label, role.launch.source_id
                    ));
                }
            }
        }
    }

    /// Recheck a provider after an agent error. A positive quota result is
    /// handled here so callers can suppress the ordinary failure message.
    pub async fn observe_failure(&self, role: &ResolvedAgent) -> bool {
        match self.gate.refresh(role).await {
            Check::NearLimit { resets_at } => {
                self.handle_near_limit(role, resets_at);
                true
            }
            Check::Clear | Check::Unavailable => false,
        }
    }

    /// Returns true when the current role moved to a fallback.
    fn handle_near_limit(&self, failed: &ResolvedAgent, resets_at: Option<i64>) -> bool {
        let provider = failed.launch.source_id.clone();
        let mut state = self.state.lock().expect("role pool poisoned");
        state.excluded_providers.insert(provider.clone());
        if self.roles[state.current].launch.source_id != provider {
            return true;
        }
        let next = self.auto_failover.then(|| {
            self.roles.iter().enumerate().find(|(_, candidate)| {
                !state
                    .excluded_providers
                    .contains(&candidate.launch.source_id)
            })
        });
        if let Some((next, replacement)) = next.flatten() {
            state.current = next;
            state.announced_block = false;
            let _ = self.ui_tx.send(UiEvent::Info(format!(
                "{} quota guard switched {} via {} to {} via {}",
                self.label,
                failed.model.model,
                failed.launch.source_id,
                replacement.model.model,
                replacement.launch.source_id,
            )));
            let _ = self.ui_tx.send(UiEvent::SubagentPoolModelChanged {
                model: replacement.model.model.clone(),
                source_id: replacement.launch.source_id.clone(),
            });
            return true;
        }
        if !state.announced_block {
            let reset = resets_at
                .and_then(mj_core::usage_format::format_reset_local_seconds)
                .map(|value| format!(" until {value}"))
                .unwrap_or_default();
            let _ = self.ui_tx.send(UiEvent::Warning(format!(
                "{} paused: {} quota has 5% or less remaining{}",
                self.label, provider, reset
            )));
            state.announced_block = true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::claude_usage::ClaudeUsageWindow;
    use mj_core::codex_usage::CodexUsageWindow;
    use mj_core::deepswe::Row;
    use mj_core::roster::{AdapterKind, AdapterLaunch};

    fn role(model: &str, source_id: &str, kind: AdapterKind) -> ResolvedAgent {
        ResolvedAgent {
            model: Row {
                model: model.into(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: model.into(),
            launch: AdapterLaunch {
                kind,
                source_id: source_id.into(),
                command: PathBuf::from(source_id),
                args: Vec::new(),
                env: HashMap::new(),
            },
            ranked: true,
            reasoning_effort: None,
        }
    }

    #[test]
    fn claude_any_window_at_five_percent_is_near_limit() {
        let report = ClaudeUsageReport {
            five_hour: Some(ClaudeUsageWindow {
                remaining_percent: 5,
                reset_context: None,
            }),
            week: None,
        };
        assert_eq!(claude_check(&report), Check::NearLimit { resets_at: None });
    }

    #[test]
    fn codex_uses_earliest_near_limit_reset() {
        let report = CodexUsageReport {
            primary: Some(CodexUsageWindow {
                label: "primary".into(),
                remaining_percent: 3,
                resets_at: Some(20),
            }),
            secondary: Some(CodexUsageWindow {
                label: "secondary".into(),
                remaining_percent: 5,
                resets_at: Some(10),
            }),
        };
        assert_eq!(
            codex_check(&report),
            Check::NearLimit {
                resets_at: Some(10)
            }
        );
    }

    #[test]
    fn near_limit_advances_to_a_different_provider() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let claude = role("claude-opus", "claude-acp", AdapterKind::Claude);
        let codex = role("gpt-codex", "codex-acp", AdapterKind::Codex);
        let pool = RolePool::new(
            vec![claude.clone(), codex.clone()],
            Gate::new(PathBuf::from("."), ui_tx.clone()),
            true,
            "subagents",
            ui_tx,
        );

        assert!(pool.handle_near_limit(&claude, None));
        assert_eq!(pool.current().launch.source_id, codex.launch.source_id);
        assert!(matches!(ui_rx.try_recv(), Ok(UiEvent::Info(_))));
        assert!(matches!(
            ui_rx.try_recv(),
            Ok(UiEvent::SubagentPoolModelChanged { .. })
        ));
    }

    #[test]
    fn disabled_failover_coalesces_block_warnings() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let claude = role("claude-opus", "claude-acp", AdapterKind::Claude);
        let pool = RolePool::new(
            vec![claude.clone()],
            Gate::new(PathBuf::from("."), ui_tx.clone()),
            false,
            "subagents",
            ui_tx,
        );

        assert!(!pool.handle_near_limit(&claude, None));
        assert!(!pool.handle_near_limit(&claude, None));
        assert!(matches!(ui_rx.try_recv(), Ok(UiEvent::Warning(_))));
        assert!(ui_rx.try_recv().is_err());
    }

    #[test]
    fn near_limit_for_a_non_current_provider_keeps_the_selection_quietly() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let claude = role("claude-opus", "claude-acp", AdapterKind::Claude);
        let codex = role("gpt-codex", "codex-acp", AdapterKind::Codex);
        let pool = RolePool::new(
            vec![claude.clone(), codex.clone()],
            Gate::new(PathBuf::from("."), ui_tx.clone()),
            true,
            "subagents",
            ui_tx,
        );

        // The fallback seat hit its limit while the current seat is
        // fine: it is excluded for later failover, nothing else changes.
        assert!(pool.handle_near_limit(&codex, None));
        assert_eq!(pool.current().launch.source_id, claude.launch.source_id);
        assert!(ui_rx.try_recv().is_err());
    }

    #[test]
    fn exhausted_failover_announces_the_block_with_reset_time() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let claude = role("claude-opus", "claude-acp", AdapterKind::Claude);
        let codex = role("gpt-codex", "codex-acp", AdapterKind::Codex);
        let pool = RolePool::new(
            vec![claude.clone(), codex.clone()],
            Gate::new(PathBuf::from("."), ui_tx.clone()),
            true,
            "subagents",
            ui_tx,
        );

        assert!(pool.handle_near_limit(&claude, None));
        while ui_rx.try_recv().is_ok() {}

        // Every provider is now excluded, so the pool blocks and names
        // the reset time in its warning.
        assert!(!pool.handle_near_limit(&codex, Some(0)));
        match ui_rx.try_recv() {
            Ok(UiEvent::Warning(text)) => {
                assert!(text.contains("codex-acp"), "unexpected warning: {text}");
                assert!(text.contains(" until "), "missing reset time: {text}");
            }
            other => panic!("expected block warning, got {other:?}"),
        }
        assert!(ui_rx.try_recv().is_err());
    }
}
