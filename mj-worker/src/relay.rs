//! Persistent, transport-neutral protocol core for a Hel target worker.
//!
//! The worker never listens on a network port. Controllers speak newline-
//! delimited JSON through `mj worker proxy`, which can itself be carried over
//! SSH or a container exec stream.
//!
//! This module is the root of the relay implementation. It keeps the
//! `DurableRelay` type itself: opening and recovering a session, the
//! operational-state and activity clocks, and recording observations and
//! session updates. The rest of the implementation sits in sibling modules:
//! [`requests`] answers relay envelopes, [`commands`] owns the command queue
//! and the claim scheduler, [`background`] tracks agent-owned background work
//! and terminals, [`replay`] serves replay pages and cursor validation, and
//! [`journal`] does durable journal I/O. The wire protocol and the
//! deterministic event/snapshot state machine are defined in mj-core and used
//! by both runtime owners.

pub use mj_core::relay::*;
mod background;
mod commands;
mod journal;
mod native_history;
mod replay;
mod requests;
mod serving;
mod verdict;
use background::{KimiProvisionalTask, KimiTaskEntry, is_agent_output};
use commands::validate_identifier;
pub use replay::{DeferredRelayAttach, RelayReplayPlan};
pub use serving::serve_relay_json_lines;
#[cfg(test)]
mod capacity_tests;
#[cfg(test)]
mod snapshot_tests;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, anyhow, bail};

use journal::{
    JournalReadMode, RelayJournalSpan, open_relay_journal, read_restored_relay_seed,
    visit_relay_journal_file,
};
use mj_checkpoint::archive::CanonicalQueuedCommandKind;
use mj_core::clock::epoch_millis;
/// Whether this relay models turns the harness starts on its own.
///
/// Only Claude Code's adapter marks the end of such a turn today (an origin
/// key on the settling `usage_update`), so every other harness keeps the
/// behaviour it had before harness turns existed. See
/// `.agents/docs/claude-autonomous-turns.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HarnessTurnPolicy {
    #[default]
    Disabled,
    ClaudeAdapter,
    CodexAdapter,
}

/// Where this relay learns about commands the agent left running.
///
/// Claude reports its own background tasks. Kimi uses Hel's terminals for
/// shells and its native task journal for detached agents. Codex runs shells
/// itself and only reports them as tool cards, so for Codex the evidence is a
/// card whose result carries no exit code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BackgroundWorkPolicy {
    /// Live terminals Hel spawned for the agent.
    #[default]
    HostedTerminals,
    /// Claude's live background-task set, together with hosted terminals.
    ClaudeTasks,
    /// Kimi's native detached-agent set, together with hosted terminals.
    KimiTasks,
    /// `exec_command` cards whose result has an explicitly null exit code.
    CodexExecCards,
}

/// Durable, session-side ACP store-and-forward relay.
///
/// `RelaySnapshot` is the canonical operational state. Journal spans retain
/// observations newer than the last frontier covered by both a durable
/// controller acknowledgement and a verified checkpoint.
pub struct DurableRelay {
    root: PathBuf,
    relay_version: String,
    /// Content address of the executable serving this relay, reported in
    /// hello. The crate version cannot tell two builds of the same release
    /// apart, and a controller has to know whether the worker it is talking to
    /// is the binary it would install today.
    worker_build: Option<String>,
    /// Current ACP process readiness; never recovered from the journal.
    acp_ready: bool,
    checkpoint_only: bool,
    /// Optional extension advertised by the current ACP process.
    steering_supported: Option<bool>,
    snapshot: RelaySnapshot,
    /// Canonical, non-overlapping slices of the durable journal. Event bodies
    /// stay on disk; only enough metadata to locate a requested ordinal is
    /// retained in memory.
    journal_spans: Vec<RelayJournalSpan>,
    /// A small optimization for recent cursor validation. This is never the
    /// source of truth and is deliberately fixed-size.
    hot_events: VecDeque<RelayEvent>,
    /// Digests proven while serving older replay pages. Controllers normally
    /// return the previous page's frontier, so retaining those sparse cursors
    /// avoids decompressing that segment again merely to validate the next
    /// request. Event bodies remain on disk and authoritative.
    replay_cursors: VecDeque<(u64, String)>,
    /// Journal bytes appended since `relay-state.json` last matched this
    /// snapshot. Zero means the durable file is exactly this state.
    unpersisted_journal_bytes: usize,
    /// Bumped whenever a captured [`RelayReplayPlan`] stops describing the
    /// journal on disk: sealing moves the active segment's events into a new
    /// file, and garbage collection rewrites and prunes segments. Plain
    /// appends never bump it — they only extend the active segment, which a
    /// lock-free reader already tolerates.
    journal_generation: u64,
    acp_activity: AcpActivityClock,
    step_clock: crate::acp::StepClock,
    /// Whether agent output arriving with no prompt in flight opens a turn.
    /// The runtime sets this from the configured harness right after `open`.
    harness_turns: HarnessTurnPolicy,
    /// Where commands the agent left running are read from.
    background_work: BackgroundWorkPolicy,
    /// Tool calls that still report pending or in-progress, with the start of
    /// their current status. This is stronger foreground evidence than a
    /// harness-neutral step clock, whose prose steps have no portable ending.
    ///
    /// A shared handle rather than a private map: the turn stall watchdog
    /// reads the same tool calls, and when it could not, a turn blocked in a
    /// long build was failed as if the harness had died (#1020).
    foreground_tools: mj_core::activity::ToolsInFlight,
    turn_context: mj_transcript::turn_context::TurnContext,
    verdict_harness: Option<mj_core::config::HarnessKind>,
    replied_verdict_pending: bool,
    replied_verdict: verdict::RepliedVerdictState,
    /// Codex tool calls explicitly introduced as execute cards, with the
    /// command needed if a later partial update says the process is detached.
    codex_execute_tools: BTreeMap<String, String>,
    /// Commands a Codex exec card reported without an exit code, keyed by the
    /// tool call id so a later card for the same call clears it. In memory,
    /// like the terminals: it describes processes that are alive now.
    background_exec_cards: BTreeMap<String, BackgroundCommand>,
    /// Claude's process-local background-task level, replaced on every update.
    claude_background_tasks: BTreeMap<String, BackgroundCommand>,
    /// Claude tasks whose stop this process requested and the adapter has not
    /// yet acknowledged. The adapter answers a stop with a plain agent chunk
    /// and no model cycle, so no result would settle a harness turn opened
    /// for that chunk.
    claude_pending_stops: BTreeSet<String>,
    /// Kimi detached agents and processes confirmed by its native journal.
    kimi_background_tasks: BTreeMap<String, KimiTaskEntry>,
    /// Positive ACP launch evidence retained until the native journal
    /// correlates it through native task identity or `parentToolCallId`.
    kimi_provisional_tasks: BTreeMap<String, KimiProvisionalTask>,
    kimi_observed_task_ids: BTreeSet<String>,
    kimi_observed_tool_ids: BTreeSet<String>,
    /// Whether Kimi's provider-owned background work is synchronized. Other
    /// policies leave this absent.
    background_work_known: Option<bool>,
    /// Claude AIR tasks that the adapter currently says can be stopped.
    claude_stoppable_tasks: BTreeSet<String>,
    /// ACP terminals are connection-owned and disappear when that connection
    /// is torn down, so they belong in memory rather than the durable relay
    /// snapshot or transcript journal.
    active_agent_terminals: BTreeMap<String, ActiveAgentTerminal>,
    /// A short-lived guard against a fast child reporting close before the
    /// create handler publishes its started event on the shared channel.
    closed_agent_terminals: BTreeSet<String>,
    /// Hosted terminal each agent tool call embedded, keyed by the native tool
    /// call id. Kimi runs a detached shell in one of our terminals, so this is
    /// what ties its native task record to the terminal doing the work.
    agent_terminal_tool_calls: BTreeMap<String, String>,
    /// Benchmark aid: stage and persist a snapshot on every append, the way
    /// the relay did before transcript appends were amortized, so both
    /// policies can be timed in one process.
    #[cfg(test)]
    stage_snapshot_every_append: bool,
}

impl DurableRelay {
    pub fn open(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
    ) -> Result<Self> {
        Self::open_with_mode(root, session_id, relay_version, false)
    }

    /// Open existing durable state without promoting work onto a harness.
    pub fn open_for_checkpoint(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
    ) -> Result<Self> {
        let root = root.into();
        anyhow::ensure!(
            root.join(RELAY_STATE_FILE).is_file(),
            "checkpoint recovery requires existing relay state"
        );
        Self::open_with_mode(root, session_id, relay_version, true)
    }

    fn open_with_mode(
        root: impl Into<PathBuf>,
        session_id: impl Into<String>,
        relay_version: impl Into<String>,
        checkpoint_only: bool,
    ) -> Result<Self> {
        let root = root.into();
        let session_id = session_id.into();
        validate_identifier(&session_id, "session ID")?;
        fs::create_dir_all(root.join(RELAY_JOURNAL_DIR))
            .with_context(|| format!("create relay state directory {}", root.display()))?;

        let state_path = root.join(RELAY_STATE_FILE);
        let mut snapshot = if state_path.exists() {
            let bytes =
                fs::read(&state_path).with_context(|| format!("read {}", state_path.display()))?;
            ensure_byte_budget(bytes.len(), RELAY_SNAPSHOT_BYTE_BUDGET, "relay snapshot")?;
            let mut snapshot: RelaySnapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", state_path.display()))?;
            if snapshot.format_version > RELAY_STATE_VERSION {
                bail!(
                    "relay state schema {} is newer than supported schema {RELAY_STATE_VERSION}",
                    snapshot.format_version
                );
            }
            // Upgrade an older snapshot in place. The stored frontier digests
            // stay valid — each is recomputed with the formula matching the
            // record's format — so only the schema marker advances; the next
            // persist writes it back at the current version.
            snapshot.format_version = RELAY_STATE_VERSION;
            // Older snapshots can still identify a live turn, even though
            // they did not retain its start through subsequent background work.
            snapshot.activity_turn_started_at_ms = snapshot
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| {
                    snapshot
                        .harness_turn
                        .as_ref()
                        .map(|turn| turn.started_at_ms)
                })
                .or(snapshot.activity_turn_started_at_ms);
            if snapshot.session_id != session_id {
                bail!(
                    "relay state belongs to session {}, not {session_id}",
                    snapshot.session_id
                );
            }
            snapshot
        } else {
            let mut snapshot = RelaySnapshot::new(session_id);
            if let Some(restored) = read_restored_relay_seed(&root)? {
                snapshot.latest_ordinal = restored.event_frontier;
                snapshot.latest_digest = restored.event_frontier_digest.clone();
                snapshot.acknowledged_through = restored.event_frontier;
                snapshot.acknowledged_digest = restored.event_frontier_digest.clone();
                snapshot.recovery_floor_ordinal = restored.event_frontier;
                snapshot.recovery_floor_digest = restored.event_frontier_digest;
                // The accepted model and effort the worker pins on its first
                // bridge start, exactly as it does after a restart.
                snapshot.config.extend(restored.accepted_config);
                for queued in restored.queued_prompts {
                    validate_identifier(&queued.command_id, "restored queued command ID")?;
                    if queued.content.is_empty() {
                        bail!("restored queued command {} is empty", queued.command_id);
                    }
                    if snapshot.handled_commands.contains_key(&queued.command_id) {
                        bail!(
                            "restored canonical session contains duplicate queued command {}",
                            queued.command_id
                        );
                    }
                    let payload = match queued.kind {
                        CanonicalQueuedCommandKind::Prompt => StoredQueuedRelayPayload::Prompt {
                            prompt: queued
                                .content
                                .into_iter()
                                .map(serde_json::from_value)
                                .collect::<serde_json::Result<Vec<ContentBlock>>>()
                                .with_context(|| {
                                    format!(
                                        "decode ACP content for restored queued command {}",
                                        queued.command_id
                                    )
                                })?,
                        },
                        CanonicalQueuedCommandKind::SetConfig { key, value } => {
                            StoredQueuedRelayPayload::SetConfig { key, value }
                        }
                    };
                    let command = match &payload {
                        StoredQueuedRelayPayload::Prompt { prompt } => RelayCommand::Prompt {
                            prompt: prompt.clone(),
                        },
                        StoredQueuedRelayPayload::SetConfig { key, value } => {
                            RelayCommand::SetConfig {
                                key: key.clone(),
                                value: value.clone(),
                            }
                        }
                    };
                    snapshot.handled_commands.insert(
                        queued.command_id.clone(),
                        HandledRelayCommand {
                            command: command.clone(),
                            accepted_ordinal: restored.event_frontier,
                            terminal_ordinal: None,
                        },
                    );
                    snapshot.dispatches.insert(
                        queued.command_id.clone(),
                        RelayDispatchRecord {
                            command,
                            state: RelayDispatchState::Queued,
                        },
                    );
                    snapshot.queued_prompts.push(StoredQueuedRelayCommand {
                        command_id: queued.command_id,
                        payload,
                        created_at_ms: queued.queued_at_ms,
                    });
                }
            }
            snapshot
        };

        let assigned_store_id = snapshot.store_id.is_none();
        if assigned_store_id {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random)
                .map_err(|error| anyhow::anyhow!("generate worker store identity: {error}"))?;
            snapshot.store_id = Some(format!("{:032x}", u128::from_le_bytes(random)));
        }
        validate_relay_snapshot_frontiers(&snapshot)?;
        let retained_through = snapshot.retained_through();
        let retained_digest = snapshot.retained_digest().to_owned();
        let snapshot_ordinal = snapshot.latest_ordinal;
        let missing_native_history = snapshot.native_session_id.is_some()
            && snapshot.native_session_opened_ordinal.is_none()
            && !snapshot.native_session_used;
        let (journal_spans, hot_events) = open_relay_journal(
            &root.join(RELAY_JOURNAL_DIR),
            retained_through,
            &retained_digest,
            snapshot_ordinal,
            &mut snapshot,
        )?;
        ensure_serialized_budget(
            &snapshot.operational_state(),
            RELAY_STATE_BYTE_BUDGET,
            "relay operational state",
        )?;

        let mut relay = Self {
            root,
            relay_version: relay_version.into(),
            worker_build: None,
            acp_ready: false,
            checkpoint_only,
            steering_supported: None,
            snapshot,
            journal_spans,
            hot_events,
            replay_cursors: VecDeque::new(),
            unpersisted_journal_bytes: 0,
            journal_generation: 0,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            harness_turns: HarnessTurnPolicy::default(),
            background_work: BackgroundWorkPolicy::default(),
            foreground_tools: mj_core::activity::ToolsInFlight::default(),
            turn_context: Default::default(),
            verdict_harness: None,
            replied_verdict_pending: false,
            replied_verdict: Default::default(),
            codex_execute_tools: BTreeMap::new(),
            background_exec_cards: BTreeMap::new(),
            claude_background_tasks: BTreeMap::new(),
            claude_pending_stops: BTreeSet::new(),
            kimi_background_tasks: BTreeMap::new(),
            kimi_provisional_tasks: BTreeMap::new(),
            kimi_observed_task_ids: BTreeSet::new(),
            kimi_observed_tool_ids: BTreeSet::new(),
            background_work_known: None,
            claude_stoppable_tasks: BTreeSet::new(),
            active_agent_terminals: BTreeMap::new(),
            closed_agent_terminals: BTreeSet::new(),
            agent_terminal_tool_calls: BTreeMap::new(),
            #[cfg(test)]
            stage_snapshot_every_append: false,
        };
        let recovered_native_history =
            missing_native_history && !checkpoint_only && relay.recover_native_history_evidence();
        // Startup already reads the active journal into the bounded hot window.
        // Reuse it: old sealed history must not make worker startup slow or fail.
        if !checkpoint_only {
            let mut prompts = BTreeMap::<String, String>::new();
            if relay
                .hot_events
                .front()
                .is_none_or(|event| event.ordinal > 1)
                && relay.snapshot.latest_ordinal > 0
            {
                relay.turn_context.mark_earlier_history_omitted();
            }
            for event in &relay.hot_events {
                if let RelayObservation::CommandQueued {
                    command_id,
                    command,
                    ..
                } = &event.observation
                    && let Some(prompt) = command.prompt_blocks()
                {
                    prompts.insert(
                        command_id.clone(),
                        prompt
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                let prompt = if let Some(command_id) =
                    mj_transcript::turn_context::delivered_prompt_command_id(&event.observation)
                {
                    prompts.remove(command_id)
                } else {
                    if let RelayObservation::CommandCompleted { command_id, .. } =
                        &event.observation
                    {
                        prompts.remove(command_id);
                    }
                    None
                };
                relay
                    .turn_context
                    .observe_relay(&event.observation, prompt.as_deref());
            }
        }
        // Live-only work cannot be reconstructed on reopen. Nor can replay
        // prove an idle transition that was not yet saved with the snapshot.
        let replayed = relay.snapshot.latest_ordinal > snapshot_ordinal;
        let idle = relay.activity_is_idle();
        if relay.snapshot.activity_was_idle != Some(idle) {
            relay.snapshot.idle_since_ms = None;
            relay.snapshot.activity_was_idle = Some(idle);
        }
        if idle {
            relay.snapshot.activity_turn_started_at_ms = None;
        }
        if !state_path.exists() || replayed || assigned_store_id || recovered_native_history {
            relay.persist_snapshot()?;
        }
        relay.adopt_unqueued_queue_commands()?;
        relay.recover_nonterminal_commands()?;
        relay.promote_next_queued_command()?;
        relay.replied_verdict_pending = relay.snapshot.retry_assessment.is_some();
        Ok(relay)
    }

    /// Adopt queueable commands that an earlier relay format accepted outside
    /// the durable queue. Configuration changes used to dispatch through the
    /// control path, so a command accepted just before an upgrade would
    /// otherwise stay accepted forever without ever being promoted.
    fn adopt_unqueued_queue_commands(&mut self) -> Result<()> {
        let mut adopted: Vec<(u64, StoredQueuedRelayCommand)> = Vec::new();
        for (command_id, dispatch) in &self.snapshot.dispatches {
            if dispatch.state != RelayDispatchState::Queued
                || !dispatch.command.is_queue_entry()
                || self
                    .snapshot
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.command_id == *command_id)
            {
                continue;
            }
            let Some(handled) = self.snapshot.handled_commands.get(command_id) else {
                continue;
            };
            let payload = match &dispatch.command {
                command @ (RelayCommand::Prompt { .. }
                | RelayCommand::ContinueAuthorizedWork { .. }
                | RelayCommand::ResumeAfterQuota { .. }) => StoredQueuedRelayPayload::Prompt {
                    prompt: command
                        .prompt_blocks()
                        .expect("prompt command")
                        .into_owned(),
                },
                RelayCommand::SetConfig { key, value } => StoredQueuedRelayPayload::SetConfig {
                    key: key.clone(),
                    value: value.clone(),
                },
                _ => continue,
            };
            adopted.push((
                handled.accepted_ordinal,
                StoredQueuedRelayCommand {
                    command_id: command_id.clone(),
                    payload,
                    created_at_ms: epoch_millis(),
                },
            ));
        }
        if adopted.is_empty() {
            return Ok(());
        }
        let existing = std::mem::take(&mut self.snapshot.queued_prompts);
        let mut ordered: Vec<(u64, StoredQueuedRelayCommand)> = existing
            .into_iter()
            .map(|queued| {
                let accepted = self
                    .snapshot
                    .handled_commands
                    .get(&queued.command_id)
                    .map_or(0, |handled| handled.accepted_ordinal);
                (accepted, queued)
            })
            .collect();
        ordered.extend(adopted);
        ordered.sort_by_key(|(accepted, _)| *accepted);
        self.snapshot.queued_prompts = ordered.into_iter().map(|(_, queued)| queued).collect();
        self.persist_snapshot()
    }

    pub fn operational_state(&self) -> RelayOperationalState {
        let mut state = self.snapshot.operational_state();
        state.acp_ready = Some(self.acp_ready);
        state.checkpoint_only = self.checkpoint_only;
        state.steering_supported = self.steering_supported;
        state.last_acp_activity_at_ms = self.acp_activity.last_at_ms();
        state.current_step_started_at_ms = self.step_clock.started_at_ms();
        state.tools_in_flight = self.foreground_tools.snapshot();
        state.foreground_tool_started_at_ms = self.foreground_tools.newest_started_at_ms();
        state.active_agent_terminals = self.active_agent_terminals.values().cloned().collect();
        state.native_agent_count = self.native_agent_count();
        state.clear_context = matches!(
            self.verdict_harness,
            Some(mj_core::config::HarnessKind::Codex | mj_core::config::HarnessKind::Claude)
        );
        state.background_commands = self.background_commands();
        state.background_work_known = self.background_work_known;
        // Answer the activity question once, here, where every fact is in
        // hand. Consumers read this instead of each deriving their own.
        let facts = self.activity_facts();
        state.expected_continuation = facts.expected_continuation;
        state.inferred_idle_since_ms = facts.inferred_idle_since_ms;
        state.activity = Some(mj_core::activity::classify(&facts));
        state
    }

    /// A stopped bridge cannot become ready again until its replacement configures.
    pub fn clear_acp_readiness(&mut self) {
        self.acp_ready = false;
        self.steering_supported = None;
    }

    /// Publish the current harness's steering support without changing the journal.
    pub fn set_steering_supported(&mut self, supported: Option<bool>) {
        self.steering_supported = supported;
    }

    /// Every fact that bears on whether this session is working.
    ///
    /// The idle clock runs on every journal append, so this assembles the
    /// facts directly rather than through a whole operational state, which
    /// would clone the session's configuration once per streamed chunk. It
    /// must stay identical to `RelayOperationalState::facts`, which is what
    /// the daemon reads; `worker_facts_match_the_published_state` pins them
    /// together.
    fn clear_context_started_at_ms(&self) -> Option<i64> {
        self.clear_context_in_progress()
            .then_some(self.snapshot.activity_turn_started_at_ms)
            .flatten()
    }

    pub fn activity_facts(&self) -> mj_core::activity::ActivityFacts {
        let background_commands = self.background_commands().len() + self.native_agent_count();
        self.turn_context
            .set_counts(background_commands, self.snapshot.queued_prompts.len());
        self.turn_context.set_background_inventory(
            self.background_commands(),
            self.snapshot
                .native_agents
                .iter()
                .filter(|(_, agent)| {
                    agent.state == mj_core::native_agent::NativeAgentState::Running
                })
                .map(|(id, _)| id.clone())
                .collect(),
        );
        let mut facts = mj_core::activity::ActivityFacts {
            execution: self.snapshot.execution,
            prompt_started_at_ms: self
                .snapshot
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| self.clear_context_started_at_ms()),
            harness_turn_started_at_ms: self.snapshot.harness_turn.map(|turn| turn.started_at_ms),
            turn_started_at_ms: self.snapshot.activity_turn_started_at_ms,
            expected_continuation: None,
            inferred_idle_since_ms: None,
            queued_commands: self.snapshot.queued_prompts.len(),
            tools_in_flight: self.foreground_tools.snapshot(),
            background_started_at_ms: self
                .background_commands()
                .iter()
                .map(|command| command.started_at_ms)
                .chain(
                    self.snapshot
                        .active_user_shells
                        .values()
                        .filter_map(|shell| shell.started_at_ms),
                )
                .min(),
            background_commands: self.background_commands().len() + self.native_agent_count(),
            active_user_shells: self.snapshot.active_user_shells.len(),
            active_agent_terminals: self.active_agent_terminals.len(),
            goal_active: self.snapshot.goal.active(),
            goal_running: self.snapshot.goal.running(),
            goal_pending_resume: self.snapshot.goal.pending_resume.is_some(),
            goal_decision: self.snapshot.goal.decision.is_some(),
            goal_synchronized: self.snapshot.goal.synchronized(),
            background_work_known: self.background_work_known,
            acp_ready: Some(self.acp_ready),
            checkpoint_only: self.checkpoint_only,
            checkpoint_barrier: self.snapshot.checkpoint_barrier.is_some(),
            capacity_retry_armed: self
                .snapshot
                .capacity_retry
                .as_ref()
                .is_some_and(|retry| !retry.submitted),
            last_acp_activity_at_ms: self.acp_activity.last_at_ms(),
            current_step_started_at_ms: self.step_clock.started_at_ms(),
            idle_since_ms: self.snapshot.idle_since_ms,
        };
        (facts.expected_continuation, facts.inferred_idle_since_ms) = self
            .replied_verdict
            .inference(self.turn_context.generation(), &facts);
        facts
    }

    /// Whether the session's idle clock should be running.
    ///
    /// The same classification every other part of Mjolnir uses. This was a
    /// private duplicate of it, and it disagreed: it counted neither queued
    /// commands nor open terminals, so the worker could publish "idle since"
    /// about a session the controller called busy.
    fn activity_is_idle(&self) -> bool {
        mj_core::activity::classify(&self.activity_facts()).is_idle()
    }

    /// Update only at activity mutations, never when a viewer reads status.
    fn refresh_idle_clock(&mut self, now_ms: i64) -> bool {
        let idle = self.activity_is_idle();
        let previous = self.snapshot.activity_was_idle;
        if previous == Some(idle) {
            return false;
        }
        self.snapshot.idle_since_ms = (idle && previous == Some(false)).then_some(now_ms);
        if idle {
            self.snapshot.activity_turn_started_at_ms = None;
        }
        self.snapshot.activity_was_idle = Some(idle);
        true
    }

    fn persist_activity_transition(&mut self) -> Result<()> {
        if self.refresh_idle_clock(epoch_millis()) {
            self.persist_snapshot()
                .context("persist session idle transition")?;
        }
        Ok(())
    }

    /// Record the content address of the executable serving this relay, so
    /// hello can report which build a controller reached.
    pub fn set_worker_build(&mut self, digest: Option<String>) {
        self.worker_build = digest;
    }

    /// Choose whether agent output with no prompt in flight opens a turn.
    pub fn set_harness_turn_policy(&mut self, policy: HarnessTurnPolicy) {
        self.harness_turns = policy;
    }

    pub fn set_turn_verdict_harness(&mut self, harness: mj_core::config::HarnessKind) {
        self.verdict_harness = Some(harness);
        if self.turn_context.decision_log().is_none() {
            match mj_core::jev::DecisionLog::open(self.root.join("jev-decisions")) {
                Ok(log) => self.turn_context.set_decision_log(log),
                Err(error) => tracing::warn!(%error, "Jev diagnostic log unavailable"),
            }
        }
        self.turn_context.set_session_id(&self.snapshot.session_id);
    }

    pub fn turn_context(&self) -> mj_transcript::turn_context::TurnContext {
        self.turn_context.clone()
    }

    pub fn acp_activity_clock(&self) -> AcpActivityClock {
        self.acp_activity.clone()
    }

    pub fn step_clock(&self) -> crate::acp::StepClock {
        self.step_clock.clone()
    }

    /// The tool calls the agent has open. Shared, not copied: the ACP driver's
    /// stall watchdog reads exactly what this relay records.
    pub fn tools_in_flight(&self) -> mj_core::activity::ToolsInFlight {
        self.foreground_tools.clone()
    }

    /// The directory holding this relay's durable state.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn latest_ordinal(&self) -> u64 {
        self.snapshot.latest_ordinal
    }

    pub fn latest_digest(&self) -> &str {
        &self.snapshot.latest_digest
    }

    pub fn acknowledged_through(&self) -> u64 {
        self.snapshot.acknowledged_through
    }

    pub fn acknowledged_digest(&self) -> &str {
        &self.snapshot.acknowledged_digest
    }

    pub fn events_after(&self, after_ordinal: u64, after_digest: &str) -> Result<Vec<RelayEvent>> {
        let plan = self.replay_plan();
        plan.validate_cursor(after_ordinal, after_digest)?;
        plan.read_events_after(after_ordinal, after_digest, usize::MAX)
            .map(|page| page.events)
    }

    /// Whether this session's native thread may hold conversation history
    /// Mjolnir cannot see. Computed from snapshot state alone, with no journal
    /// replay, and deliberately conservative: it answers `false` only for a
    /// thread this journal created and that nothing has used yet.
    ///
    /// A `false` answer is what allows Mjolnir to replace a native session the
    /// harness says it has no record of. Codex writes a thread's rollout, and
    /// Claude Code a session's transcript, only at the first user message, so
    /// a session that was opened and never prompted can be missing on disk.
    pub fn native_session_may_have_history(&self) -> bool {
        // Set when the agent sent conversation content, when a prompt was
        // transmitted, when the session was resumed rather than created here,
        // or when its identity arrived from outside this journal.
        if self.snapshot.native_session_used {
            return true;
        }
        // History before the recovery floor was released to an archive, so the
        // snapshot no longer describes everything this session did. That only
        // hides history belonging to the *current* native session if that
        // session could have existed then. A session this journal opened above
        // the floor has every event about it above the floor too, so the
        // archive cannot hold any of its content. An unknown opening ordinal —
        // an older snapshot, or no session opened yet — cannot be placed
        // against the floor, so it counts as history.
        match self.snapshot.native_session_opened_ordinal {
            Some(opened) if opened > self.snapshot.recovery_floor_ordinal => {}
            _ => return true,
        }
        // A prompt that only waits in the durable queue never reached the
        // agent. Anything past admission may have.
        self.snapshot
            .dispatches
            .values()
            .any(native_history::prompt_may_have_reached_agent)
    }

    /// Record that the native thread has been used and can never be replaced.
    /// Persisted immediately: the evidence is live ACP traffic, and a worker
    /// that dies right after it must not come back believing the thread empty.
    pub fn mark_native_session_used(&mut self) -> Result<()> {
        if self.snapshot.native_session_used {
            return Ok(());
        }
        self.snapshot.native_session_used = true;
        self.persist_snapshot()
    }

    /// How many times the journal's files stopped matching a captured
    /// [`RelayReplayPlan`]. A lock-free reader compares this before and after
    /// its read to tell a stale plan from a real desynchronization.
    pub fn journal_generation(&self) -> u64 {
        self.journal_generation
    }

    pub fn record_observation(&mut self, observation: RelayObservation) -> Result<u64> {
        let acp_ready = match &observation {
            RelayObservation::SessionConfigured { .. } => Some(true),
            RelayObservation::AgentInitialized { .. }
            | RelayObservation::SessionRestarted
            | RelayObservation::Closing
            | RelayObservation::Closed => Some(false),
            _ => None,
        };
        // A restart or a close ends the harness process that owned whatever it
        // had left running, so nothing it reported is still alive.
        if matches!(
            observation,
            RelayObservation::SessionRestarted
                | RelayObservation::Closing
                | RelayObservation::Closed
        ) {
            self.forget_harness_processes();
        }
        let ordinal = self.append_relay_event(None, observation)?;
        if let Some(ready) = acp_ready {
            self.acp_ready = ready;
        }
        Ok(ordinal)
    }

    pub fn record_session_update(&mut self, mut update: SessionUpdate) -> Result<u64> {
        // An ACP file edit arrives as the whole file before and the whole file
        // after. Store the patch between them instead: nothing downstream
        // reconstructs a file from those copies, and the patch is proportional
        // to the edit rather than to the file. See `mj_core::diff`.
        match &mut update {
            SessionUpdate::ToolCall(call) => {
                mj_core::diff::compact_tool_call_content(&mut call.content);
            }
            SessionUpdate::ToolCallUpdate(call) => {
                if let Some(content) = call.fields.content.as_mut() {
                    mj_core::diff::compact_tool_call_content(content);
                }
            }
            _ => {}
        }
        let native_before = self.snapshot.goal.running();
        let mut native_after = self.snapshot.goal.clone();
        native_after.apply(&update)?;
        let codex = self.harness_turns == HarnessTurnPolicy::CodexAdapter;
        if codex && !native_before && native_after.running() {
            self.append_relay_event(
                None,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        let claude = self.harness_turns == HarnessTurnPolicy::ClaudeAdapter;
        // A stop we requested is acknowledged with a plain chunk and nothing
        // else: no model cycle runs, so no result would ever settle a turn
        // opened for it. Consume the expectation and keep the line.
        let ack = claude && self.is_claude_stop_acknowledgement(&update);
        if ack {
            self.claude_pending_stops.pop_first();
        }
        if claude && !ack && self.opens_harness_turn(&update) {
            self.append_relay_event(
                None,
                RelayObservation::HarnessTurnStarted {
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        self.foreground_tools.observe_with_start(
            &update,
            self.step_clock.started_at_ms().unwrap_or_else(epoch_millis),
        );
        if self.background_work == BackgroundWorkPolicy::CodexExecCards {
            self.track_codex_exec_card(&update);
        }
        if self.background_work == BackgroundWorkPolicy::KimiTasks {
            self.track_agent_terminal_tool_call(&update);
            self.track_kimi_background_agent(&update);
        }
        let ordinal = self.record_observation(RelayObservation::SessionUpdate {
            update: Box::new(update),
        })?;
        // A Claude turn settles on its SDK result instead, in
        // `claude_turn_result`: the adapter's origin marker is left out when a
        // cycle produced no assistant usage.
        if codex && native_before && !native_after.running() && self.snapshot.harness_turn.is_some()
        {
            self.settle_harness_turn(Some("codex".into()))?;
        }
        Ok(ordinal)
    }

    /// Claude Code ended a model cycle that did not end one of Mjolnir's
    /// prompts. If a turn Claude Code started on its own is open, this result
    /// is its boundary, whatever started the cycle.
    pub fn claude_turn_result(&mut self, result: &mj_core::acp::ClaudeTurnResult) -> Result<()> {
        if self.harness_turns != HarnessTurnPolicy::ClaudeAdapter
            || self.snapshot.harness_turn.is_none()
        {
            return Ok(());
        }
        self.settle_harness_turn(Some(
            result
                .origin_kind
                .clone()
                .unwrap_or_else(|| "human".to_owned()),
        ))
    }

    fn settle_harness_turn(&mut self, origin: Option<String>) -> Result<()> {
        self.append_relay_event(
            None,
            RelayObservation::HarnessTurnSettled {
                origin,
                // The projection cannot see `active_prompt`, so the event has
                // to carry whether a prompt of ours is still running.
                prompt_in_flight: self.snapshot.active_prompt.is_some(),
            },
        )?;
        self.finish_turn_activity()?;
        self.replied_verdict_pending = true;
        Ok(())
    }

    /// Forget provisional tool statuses at a boundary the harness itself has
    /// confirmed. Independently tracked background work survives into idle.
    fn finish_turn_activity(&mut self) -> Result<()> {
        self.foreground_tools.clear();
        self.codex_execute_tools
            .retain(|tool_call_id, _| self.background_exec_cards.contains_key(tool_call_id));
        self.persist_activity_transition()
    }

    /// Whether this update reveals the harness working with nothing of Hel's
    /// in flight, which is what opens a harness-initiated turn.
    fn opens_harness_turn(&self, update: &SessionUpdate) -> bool {
        is_agent_output(update)
            && self.snapshot.active_prompt.is_none()
            && self.snapshot.harness_turn.is_none()
            && !matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
    }

    fn digest_at(&self, ordinal: u64) -> Result<Option<String>> {
        self.replay_plan().digest_at(ordinal)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

    pub(crate) fn relay_request(request_id: &str, request: RelayRequest) -> RelayRequestEnvelope {
        RelayRequestEnvelope {
            request_id: request_id.to_owned(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request,
        }
    }

    pub(crate) fn submit_relay(
        relay: &mut DurableRelay,
        command_id: &str,
        command: RelayCommand,
    ) -> u64 {
        let response = relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command,
            },
        ));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Accepted {
                    command_id: accepted,
                    ordinal,
                },
        } = response.body
        else {
            panic!("expected accepted relay command, got {:?}", response.body);
        };
        assert_eq!(accepted, command_id);
        ordinal
    }

    pub(crate) fn attach_relay(
        relay: &mut DurableRelay,
        request_id: &str,
        after_ordinal: u64,
    ) -> RelayResponseEnvelope {
        let after_digest = relay.digest_at(after_ordinal).unwrap().unwrap();
        relay.handle(relay_request(
            request_id,
            RelayRequest::Attach {
                after_ordinal,
                after_digest,
            },
        ))
    }

    pub(crate) fn acknowledge_relay(
        relay: &mut DurableRelay,
        request_id: &str,
        through_ordinal: u64,
    ) -> RelayResponseEnvelope {
        let through_digest = relay.digest_at(through_ordinal).unwrap().unwrap();
        relay.handle(relay_request(
            request_id,
            RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            },
        ))
    }

    pub(crate) fn prompt(text: &str) -> RelayCommand {
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from(text)],
        }
    }

    pub(crate) fn set_config(key: &str, value: &str) -> RelayCommand {
        RelayCommand::SetConfig {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }

    pub(crate) fn queued_command_ids(relay: &DurableRelay) -> Vec<String> {
        relay
            .operational_state()
            .queued_prompts
            .into_iter()
            .map(|queued| queued.command_id)
            .collect()
    }

    pub(crate) fn finish_prompt(relay: &mut DurableRelay, command_id: &str) {
        relay
            .record_command_completed(
                command_id,
                RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
    }

    pub(crate) fn retained_events(relay: &DurableRelay) -> Vec<RelayEvent> {
        relay
            .events_after(
                relay.snapshot.retained_through(),
                relay.snapshot.retained_digest(),
            )
            .unwrap()
    }

    pub(crate) fn submit_release(
        relay: &mut DurableRelay,
        command_id: &str,
        barrier_command_id: &str,
    ) -> RelayResponseEnvelope {
        relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command: RelayCommand::ReleaseCheckpoint {
                    barrier_command_id: barrier_command_id.to_owned(),
                },
            },
        ))
    }

    pub(crate) fn submit_floor(
        relay: &mut DurableRelay,
        command_id: &str,
        through: RelayCursor,
    ) -> RelayResponseEnvelope {
        relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command: RelayCommand::AdvanceRecoveryFloor { through },
            },
        ))
    }

    pub(crate) fn ready_checkpoint(relay: &mut DurableRelay, command_id: &str) -> RelayCursor {
        submit_relay(
            relay,
            command_id,
            RelayCommand::BeginCheckpoint { reason: None },
        );
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, command_id);
        relay.record_checkpoint_ready(command_id).unwrap();
        relay.operational_state().checkpoint_ready.unwrap()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod attachment_tests;

#[cfg(test)]
mod continuation_tests;
