//! Persistent, transport-neutral protocol core for a Hel target worker.
//!
//! The worker never listens on a network port. Controllers speak newline-
//! delimited JSON through `mj worker proxy`, which can itself be carried over
//! SSH or a container exec stream.
//!
//! This module is the root of the relay implementation and keeps the
//! `DurableRelay` request-handling and scheduling core: opening/recovering a
//! session, handling requests, and the command-claim scheduler. The wire
//! protocol lives in [`protocol`], the deterministic event/snapshot state
//! machine lives in [`snapshot`], durable journal I/O lives in [`journal`],
//! Shared contracts are defined in mj-core and used by both runtime owners.

pub use mj_core::relay::*;
mod journal;
mod serving;
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

fn background_task_id(kind: &str, provider_id: &str) -> String {
    format!("{kind}:{provider_id}")
}

/// A Kimi native task together with the ACP launcher call it came from, so a
/// hosted terminal bound to that call is not listed as a second job.
#[derive(Debug, Clone)]
struct KimiTaskEntry {
    command: BackgroundCommand,
    parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
struct KimiProvisionalTask {
    command: BackgroundCommand,
    native_task_id: Option<String>,
}

/// `_meta` key the Claude adapter puts on the `usage_update` that settles a
/// turn. Its value is an object with a `kind` naming the origin.
const CLAUDE_ORIGIN_META_KEY: &str = "_claude/origin";

/// The origin kind on a settling `usage_update`, if the marker is present.
///
/// The outer option says whether the update carries the marker at all; the
/// inner one is the origin kind, which is only ever reported for diagnostics.
fn claude_turn_origin(update: &SessionUpdate) -> Option<Option<String>> {
    let SessionUpdate::UsageUpdate(usage) = update else {
        return None;
    };
    let origin = usage.meta.as_ref()?.get(CLAUDE_ORIGIN_META_KEY)?;
    Some(
        origin
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    )
}

/// Whether this update is agent output, which is what reveals that the
/// harness is working. Everything else is session bookkeeping the harness
/// also sends while idle.
fn is_agent_output(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
    )
}

/// How the Claude adapter begins the plain `agent_message_chunk` it publishes
/// when a user-requested stop ends a background task. The SDK injects nothing
/// into the model for that stop, so no turn and no origin marker follow it.
/// Only the prefix is stable: the name that follows is whatever the adapter
/// currently calls the task, which later level and start messages rename.
/// Checked against claude-agent-acp 0.73.0 `dist/async-tasks.js`
/// (`taskStopped`) on 2026-09-14.
const CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX: &str = "**Task stopped by user:** ";

/// The text of an agent message chunk whose content is a single text block.
fn agent_chunk_text(update: &SessionUpdate) -> Option<&str> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => Some(&text.text),
            _ => None,
        },
        _ => None,
    }
}

/// The command an exec card ran, from its raw input. Codex sends either a
/// string or the argv it executed.
fn exec_card_command(raw_input: Option<&serde_json::Value>) -> Option<String> {
    let command = raw_input?.get("command")?;
    if let Some(text) = command.as_str() {
        return Some(text.to_owned());
    }
    let argv = command.as_array()?;
    let words: Vec<&str> = argv.iter().filter_map(serde_json::Value::as_str).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn kimi_native_tool_call_id(tool_call_id: &str) -> &str {
    tool_call_id
        .rsplit_once(':')
        .map_or(tool_call_id, |(_, native)| native)
}

fn kimi_running_task_id(raw_output: Option<&serde_json::Value>) -> Option<&str> {
    let output = raw_output.and_then(|output| {
        output
            .as_str()
            .or_else(|| output.get("output").and_then(serde_json::Value::as_str))
    })?;
    let running = output.lines().any(|line| line.trim() == "status: running");
    if !running {
        return None;
    }
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("task_id:")
            .map(str::trim)
            .filter(|task_id| !task_id.is_empty())
    })
}

fn kimi_agent_description(
    title: Option<&str>,
    raw_input: Option<&serde_json::Value>,
    task_id: Option<&str>,
) -> String {
    raw_input
        .and_then(|input| input.get("description"))
        .and_then(serde_json::Value::as_str)
        .filter(|description| !description.trim().is_empty())
        .or(title)
        .or(task_id)
        .unwrap_or("Kimi background agent")
        .to_owned()
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
    capacity_response: CapacityResponse,
    /// Tool calls that still report pending or in-progress, with the start of
    /// their current status. This is stronger foreground evidence than a
    /// harness-neutral step clock, whose prose steps have no portable ending.
    foreground_tools: BTreeMap<String, (agent_client_protocol::schema::v1::ToolCallStatus, i64)>,
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
    /// and no origin marker, so that chunk must not open a harness turn.
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
            capacity_response: CapacityResponse::default(),
            foreground_tools: BTreeMap::new(),
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
        if !state_path.exists() || replayed || assigned_store_id {
            relay.persist_snapshot()?;
        }
        relay.adopt_unqueued_queue_commands()?;
        relay.recover_nonterminal_commands()?;
        relay.promote_next_queued_command()?;
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
                RelayCommand::Prompt { prompt } => StoredQueuedRelayPayload::Prompt {
                    prompt: prompt.clone(),
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
        state.foreground_tool_started_at_ms = self
            .foreground_tools
            .values()
            .map(|(_, started_at_ms)| *started_at_ms)
            .max();
        state.active_agent_terminals = self.active_agent_terminals.values().cloned().collect();
        state.background_commands = self.background_commands();
        state.background_work_known = self.background_work_known;
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

    fn activity_is_idle(&self) -> bool {
        self.snapshot.execution == RelayExecutionState::Idle
            && self.background_work_known != Some(false)
            && self.snapshot.active_prompt.is_none()
            && self.snapshot.harness_turn.is_none()
            && !self.snapshot.goal.active()
            && self.foreground_tools.is_empty()
            && self.snapshot.active_user_shells.is_empty()
            && self.background_commands().is_empty()
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

    /// Choose where commands the agent left running are read from.
    pub fn set_background_work_policy(&mut self, policy: BackgroundWorkPolicy) {
        self.background_work = policy;
        self.background_work_known = (policy == BackgroundWorkPolicy::KimiTasks).then_some(false);
    }

    /// Commands the agent left running with nothing waiting on them, oldest
    /// first.
    ///
    /// A hosted terminal only counts while nothing of ours is in flight: until
    /// then it is the turn's own work, not something left behind. A Codex exec
    /// card counts from the moment its result arrives with a null exit code,
    /// because Codex starts these during a turn and never mentions them again.
    ///
    /// Kimi is the exception to the in-flight rule: it detaches a shell into
    /// one of our terminals and keeps working, so a terminal its native journal
    /// calls detached is listed mid-turn. That terminal then represents the
    /// job, and the Kimi task entry behind it is left out.
    fn background_commands(&self) -> Vec<BackgroundCommand> {
        let turn_in_flight =
            self.snapshot.active_prompt.is_some() || self.snapshot.harness_turn.is_some();
        let detached_terminals = self.detached_agent_terminals();
        let mut commands: Vec<BackgroundCommand> = match self.background_work {
            BackgroundWorkPolicy::HostedTerminals
            | BackgroundWorkPolicy::ClaudeTasks
            | BackgroundWorkPolicy::KimiTasks => self
                .active_agent_terminals
                .values()
                .filter(|terminal| {
                    !turn_in_flight || detached_terminals.contains(terminal.terminal_id.as_str())
                })
                .map(|terminal| BackgroundCommand {
                    id: background_task_id("terminal", &terminal.terminal_id),
                    started_at_ms: terminal.started_at_ms,
                    command: terminal.command.clone(),
                    can_stop: true,
                })
                .collect(),
            BackgroundWorkPolicy::CodexExecCards => {
                self.background_exec_cards.values().cloned().collect()
            }
        };
        if self.background_work == BackgroundWorkPolicy::ClaudeTasks {
            commands.extend(
                self.claude_background_tasks
                    .iter()
                    .map(|(task_id, command)| {
                        let mut command = command.clone();
                        command.can_stop = self.claude_stoppable_tasks.contains(task_id);
                        command
                    }),
            );
        }
        if self.background_work == BackgroundWorkPolicy::KimiTasks {
            commands.extend(
                self.kimi_background_tasks
                    .values()
                    .filter(|entry| {
                        self.bound_agent_terminal(entry.parent_tool_call_id.as_deref())
                            .is_none()
                    })
                    .map(|entry| entry.command.clone()),
            );
            commands.extend(
                self.kimi_provisional_tasks
                    .iter()
                    .filter(|(tool_call_id, _)| {
                        self.bound_agent_terminal(Some(tool_call_id)).is_none()
                    })
                    .map(|(_, task)| task.command.clone()),
            );
        }
        commands.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.command.cmp(&right.command))
        });
        commands
    }

    /// The live hosted terminal an ACP launcher tool call embedded, if any.
    fn bound_agent_terminal(&self, tool_call_id: Option<&str>) -> Option<&str> {
        let tool_call_id = tool_call_id?;
        let terminal_id = self
            .agent_terminal_tool_calls
            .get(kimi_native_tool_call_id(tool_call_id))?;
        self.active_agent_terminals
            .contains_key(terminal_id)
            .then_some(terminal_id.as_str())
    }

    /// Terminals the agent explicitly detached, which stay listed mid-turn.
    /// Only Kimi reports this; every other policy yields an empty set.
    fn detached_agent_terminals(&self) -> BTreeSet<&str> {
        if self.background_work != BackgroundWorkPolicy::KimiTasks {
            return BTreeSet::new();
        }
        self.kimi_background_tasks
            .values()
            .filter_map(|entry| entry.parent_tool_call_id.as_deref())
            .chain(self.kimi_provisional_tasks.keys().map(String::as_str))
            .filter_map(|tool_call_id| self.bound_agent_terminal(Some(tool_call_id)))
            .collect()
    }

    /// Replace Claude's background-task level without opening a foreground
    /// turn. Edge bookends cannot be paired safely with this signal: their
    /// ordering is unspecified, and task starts also include foreground work.
    pub fn claude_background_tasks_changed(
        &mut self,
        tasks: Vec<crate::acp::ClaudeBackgroundTask>,
    ) -> Result<()> {
        if self.background_work != BackgroundWorkPolicy::ClaudeTasks
            || matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
        {
            return Ok(());
        }
        let now = epoch_millis();
        self.claude_background_tasks = tasks
            .into_iter()
            .map(|task| {
                let task_id = task.task_id;
                let started_at_ms = self
                    .claude_background_tasks
                    .get(&task_id)
                    .map_or(now, |previous| previous.started_at_ms);
                (
                    task_id.clone(),
                    BackgroundCommand {
                        id: background_task_id("claude", &task_id),
                        started_at_ms,
                        command: task.description,
                        can_stop: false,
                    },
                )
            })
            .collect();
        self.persist_activity_transition()
    }

    /// Replace Kimi's native task level and reconcile ACP launch or query
    /// evidence against task and launcher identities retained by the wire.
    pub fn kimi_background_tasks_changed(
        &mut self,
        tasks: Vec<crate::acp::KimiBackgroundTask>,
        observed_tool_call_ids: BTreeSet<String>,
        observed_task_ids: BTreeSet<String>,
    ) -> Result<()> {
        if self.background_work != BackgroundWorkPolicy::KimiTasks
            || matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
        {
            return Ok(());
        }
        self.kimi_observed_task_ids = observed_task_ids;
        self.kimi_observed_tool_ids = observed_tool_call_ids;
        self.kimi_provisional_tasks.retain(|tool_call_id, task| {
            !self
                .kimi_observed_tool_ids
                .contains(kimi_native_tool_call_id(tool_call_id))
                && !task
                    .native_task_id
                    .as_ref()
                    .is_some_and(|id| self.kimi_observed_task_ids.contains(id))
        });
        self.kimi_background_tasks = tasks
            .into_iter()
            .map(|task| {
                (
                    task.task_id.clone(),
                    KimiTaskEntry {
                        command: BackgroundCommand {
                            id: background_task_id("kimi", &task.task_id),
                            started_at_ms: task.started_at_ms,
                            command: task.description,
                            can_stop: false,
                        },
                        parent_tool_call_id: task.parent_tool_call_id,
                    },
                )
            })
            .collect();
        self.background_work_known = Some(true);
        self.persist_activity_transition()
    }

    /// A failed Kimi scan cannot prove that the previous task level ended.
    pub fn kimi_background_tasks_unavailable(&mut self) -> Result<()> {
        if self.background_work == BackgroundWorkPolicy::KimiTasks {
            self.background_work_known = Some(false);
            self.persist_activity_transition()?;
        }
        Ok(())
    }

    /// Add or remove the stop affordance announced by Claude's AIR extension.
    /// Task existence remains owned by `claude_background_tasks_changed`.
    pub fn claude_async_task_control_changed(
        &mut self,
        task_id: String,
        can_stop: bool,
    ) -> Result<()> {
        if self.background_work != BackgroundWorkPolicy::ClaudeTasks {
            return Ok(());
        }
        if can_stop {
            self.claude_stoppable_tasks.insert(task_id);
        } else {
            self.claude_stoppable_tasks.remove(&task_id);
        }
        self.persist_activity_transition()
    }

    /// Resolve a public task id only while it still names stoppable background
    /// work in this process.
    pub fn background_task_stop_target(
        &mut self,
        requested_id: &str,
    ) -> Result<BackgroundTaskStopTarget> {
        let command = self
            .background_commands()
            .into_iter()
            .find(|command| command.id == requested_id)
            .ok_or_else(|| anyhow!("background task is no longer running"))?;
        if !command.can_stop {
            bail!("this background task cannot be stopped by its agent");
        }
        if let Some((terminal_id, _)) = self
            .active_agent_terminals
            .iter()
            .find(|(id, _)| background_task_id("terminal", id) == requested_id)
        {
            return Ok(BackgroundTaskStopTarget::HostedTerminal {
                terminal_id: terminal_id.clone(),
            });
        }
        if let Some((task_id, _)) = self
            .claude_background_tasks
            .iter()
            .find(|(id, _)| background_task_id("claude", id) == requested_id)
        {
            // The adapter acknowledges the stop with a plain chunk; remember
            // the request so `record_session_update` can pair it.
            self.claude_pending_stops.insert(task_id.clone());
            return Ok(BackgroundTaskStopTarget::ClaudeAsyncTask {
                task_id: task_id.clone(),
            });
        }
        bail!("background task is no longer running")
    }

    /// Forget a Claude stop that never reached the adapter, so a later chunk
    /// with the acknowledgement prefix is not mistaken for an answer to it.
    pub fn claude_stop_not_sent(&mut self, task_id: &str) {
        self.claude_pending_stops.remove(task_id);
    }

    /// Whether this update is the adapter acknowledging a stop this process
    /// requested: a single text block with the acknowledgement prefix while a
    /// stop is pending. The adapter names no task id in the chunk and may
    /// have renamed the task since the level reported it, so the pairing is
    /// by count, not by name.
    fn is_claude_stop_acknowledgement(&self, update: &SessionUpdate) -> bool {
        !self.claude_pending_stops.is_empty()
            && agent_chunk_text(update)
                .is_some_and(|text| text.starts_with(CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX))
    }

    pub fn agent_terminal_started(&mut self, terminal: ActiveAgentTerminal) -> Result<()> {
        if self.closed_agent_terminals.remove(&terminal.terminal_id) {
            return Ok(());
        }
        self.active_agent_terminals
            .insert(terminal.terminal_id.clone(), terminal);
        self.persist_activity_transition()
    }

    pub fn agent_terminal_closed(&mut self, terminal_id: &str) -> Result<()> {
        self.active_agent_terminals.remove(terminal_id);
        self.foreground_tools
            .remove(&crate::acp::fallback_terminal_tool_call_id(terminal_id));
        self.closed_agent_terminals.insert(terminal_id.to_owned());
        self.persist_activity_transition()
    }

    pub fn clear_agent_terminals(&mut self) -> Result<()> {
        self.active_agent_terminals.clear();
        self.closed_agent_terminals.clear();
        self.forget_harness_processes();
        self.persist_activity_transition()
    }

    /// The harness that owned its processes is gone, and so is whatever it
    /// left running: a restart cannot poll a process it no longer has. Every
    /// piece of in-memory process state a harness reports is cleared here, so
    /// a restart and a closed terminal set cannot disagree about it.
    fn forget_harness_processes(&mut self) {
        self.codex_execute_tools.clear();
        self.background_exec_cards.clear();
        self.claude_background_tasks.clear();
        self.claude_pending_stops.clear();
        self.claude_stoppable_tasks.clear();
        self.kimi_background_tasks.clear();
        self.kimi_provisional_tasks.clear();
        self.kimi_observed_task_ids.clear();
        self.kimi_observed_tool_ids.clear();
        self.agent_terminal_tool_calls.clear();
        self.foreground_tools.clear();
        if self.background_work == BackgroundWorkPolicy::KimiTasks {
            self.background_work_known = Some(false);
        }
    }

    pub fn acp_activity_clock(&self) -> AcpActivityClock {
        self.acp_activity.clone()
    }

    pub fn step_clock(&self) -> crate::acp::StepClock {
        self.step_clock.clone()
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
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::Prompt { .. })
                && !matches!(
                    dispatch.state,
                    RelayDispatchState::Queued | RelayDispatchState::Pending
                )
        })
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

    /// Capture everything a replay page needs from this relay, so the file
    /// reads and gzip decompression behind it can run without the relay lock.
    fn replay_plan(&self) -> RelayReplayPlan {
        RelayReplayPlan {
            spans: self.journal_spans.clone(),
            hot_digests: self
                .hot_events
                .iter()
                .map(|event| (event.ordinal, event.digest.clone()))
                .chain(self.replay_cursors.iter().cloned())
                .collect(),
            latest_ordinal: self.snapshot.latest_ordinal,
            latest_digest: self.snapshot.latest_digest.clone(),
            acknowledged_through: self.snapshot.acknowledged_through,
            acknowledged_digest: self.snapshot.acknowledged_digest.clone(),
            recovery_floor_ordinal: self.snapshot.recovery_floor_ordinal,
            recovery_floor_digest: self.snapshot.recovery_floor_digest.clone(),
            retained_through: self.snapshot.retained_through(),
            retained_digest: self.snapshot.retained_digest().to_owned(),
            generation: self.journal_generation,
        }
    }

    /// Retain a cursor this worker just proved while reading a replay page.
    /// This is an optimization only: losing the cache causes another journal
    /// validation scan, never a loss of durable history.
    pub fn remember_replay_cursor(&mut self, response: &RelayResponseEnvelope) {
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    through_ordinal,
                    through_digest,
                    ..
                },
        } = &response.body
        else {
            return;
        };
        if self
            .replay_cursors
            .back()
            .is_some_and(|(ordinal, digest)| ordinal == through_ordinal && digest == through_digest)
        {
            return;
        }
        self.replay_cursors
            .retain(|(ordinal, _)| ordinal != through_ordinal);
        if self.replay_cursors.len() == RELAY_REPLAY_CURSOR_CAPACITY {
            self.replay_cursors.pop_front();
        }
        self.replay_cursors
            .push_back((*through_ordinal, through_digest.clone()));
    }

    /// Split an attach into the cheap part that needs the relay lock and the
    /// expensive part that does not.
    ///
    /// Catch-up over a long offline history reads page after page from disk
    /// and decompresses sealed segments. Doing that under the relay lock
    /// blocks live event recording until it finishes, which is exactly what a
    /// controller attaching is not supposed to cost the session. `None` means
    /// this envelope is not an attach, or is one that cannot be served at all;
    /// the caller falls back to [`Self::handle`], which answers it.
    pub fn take_deferred_attach(
        &self,
        envelope: &RelayRequestEnvelope,
    ) -> Option<DeferredRelayAttach> {
        let RelayRequest::Attach {
            after_ordinal,
            after_digest,
        } = &envelope.request
        else {
            return None;
        };
        if self.envelope_rejection(envelope).is_some() {
            return None;
        }
        let state = self.operational_state();
        Some(DeferredRelayAttach {
            request_id: envelope.request_id.clone(),
            protocol_version: envelope.protocol_version,
            plan: self.replay_plan(),
            state,
            after_ordinal: *after_ordinal,
            after_digest: after_digest.clone(),
        })
    }

    pub fn handle(&mut self, envelope: RelayRequestEnvelope) -> RelayResponseEnvelope {
        let request_id = envelope.request_id.clone();
        let body = self
            .handle_inner(&envelope)
            .unwrap_or_else(|error| RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: format!("{error:#}"),
                    retryable: true,
                    detail: None,
                },
            });
        let protocol_version = match &body {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello { negotiated, .. },
            } => *negotiated,
            _ => envelope.protocol_version,
        };
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body,
        }
    }

    /// Everything answered before a request reaches relay state: a usable
    /// request ID, protocol negotiation, and a method this peer's protocol
    /// version admits. `Some` is the response to send instead of handling.
    /// [`Self::take_deferred_attach`] consults the same checks so an attach it
    /// defers is one the normal path would have accepted.
    fn envelope_rejection(&self, envelope: &RelayRequestEnvelope) -> Option<RelayResponseBody> {
        if envelope.request_id.trim().is_empty() || envelope.request_id.len() > 256 {
            return Some(relay_error(
                RelayErrorCode::InvalidRequest,
                "request_id is required",
                false,
                None,
            ));
        }
        if let RelayRequest::Hello { supported, .. } = &envelope.request {
            let writer_range = RelayVersionRange {
                min: RELAY_PROTOCOL_VERSION,
                max: RELAY_PROTOCOL_VERSION,
            };
            let Some(negotiated) = writer_range.negotiate(*supported) else {
                return Some(relay_error(
                    RelayErrorCode::IncompatibleProtocol,
                    format!(
                        "controller supports {}-{}, relay supports protocol {}-{}",
                        supported.min, supported.max, writer_range.min, writer_range.max
                    ),
                    false,
                    None,
                ));
            };
            return Some(RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello {
                    negotiated,
                    relay_version: self.relay_version.clone(),
                    session_id: self.snapshot.session_id.clone(),
                    worker_build: self.worker_build.clone(),
                },
            });
        }
        mj_core::relay::protocol::relay_protocol_rejection(envelope)
    }

    fn handle_inner(&mut self, envelope: &RelayRequestEnvelope) -> Result<RelayResponseBody> {
        if let Some(body) = self.envelope_rejection(envelope) {
            return Ok(body);
        }

        let payload = match &envelope.request {
            RelayRequest::Hello { .. } => unreachable!(),
            RelayRequest::Attach {
                after_ordinal,
                after_digest,
            } => match self.attach(*after_ordinal, after_digest)? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            } => match self.acknowledge(*through_ordinal, through_digest)? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::Submit {
                command_id,
                command,
            } => match self.submit_command(command_id, command.clone())? {
                Ok(payload) => payload,
                Err(error) => return Ok(RelayResponseBody::Error { error }),
            },
            RelayRequest::Status => {
                let state = self.operational_state();
                ensure_serialized_budget(
                    &state,
                    RELAY_STATE_BYTE_BUDGET,
                    "relay operational state",
                )?;
                RelayResponsePayload::Status(state)
            }
            RelayRequest::InstallPromptContext { text } => {
                self.install_prompt_context(text.clone())?;
                RelayResponsePayload::PromptContextInstalled
            }
            RelayRequest::AttachmentPresent { .. }
            | RelayRequest::InstallAttachment { .. }
            | RelayRequest::ReadAttachment { .. }
            | RelayRequest::CredentialState
            | RelayRequest::ReadCredentials
            | RelayRequest::InstallCredentials { .. }
            | RelayRequest::SkillsState
            | RelayRequest::InstallSkills { .. }
            | RelayRequest::GithubTokenState
            | RelayRequest::InstallGithubToken { .. }
            | RelayRequest::RemoveGithubToken
            | RelayRequest::ProjectMemorySnapshot
            | RelayRequest::InstallProjectMemorySnapshot { .. }
            | RelayRequest::CompleteSubagentRequest { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "connection-only requests must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::SubagentRequests => RelayResponsePayload::SubagentRequests {
                requests: Vec::new(),
                results: Vec::new(),
            },
            RelayRequest::RespondElicitation { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "elicitation responses must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::StopBackgroundTask { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "background task stops must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
            RelayRequest::Reviewer { .. } => {
                // The reviewer has its own relay and its own harness process.
                // Only the live worker transport owns both, so this relay
                // never answers for it.
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "reviewer requests must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
        };
        Ok(RelayResponseBody::Ok { payload })
    }

    pub fn install_prompt_context(&mut self, text: String) -> Result<()> {
        if text.trim().is_empty() {
            bail!("pending prompt context is empty");
        }
        if self.snapshot.active_prompt.is_some()
            || self
                .snapshot
                .pending_prompt_context
                .as_ref()
                .is_some_and(|context| context.attached_command_id.is_some())
        {
            bail!("cannot replace prompt context while its prompt is active");
        }
        let mut next = self.snapshot.clone();
        match next.pending_prompt_context.as_mut() {
            Some(context) if context.text != text => {
                context.text.push_str("\n\n");
                context.text.push_str(&text);
            }
            Some(_) => {}
            None => {
                next.pending_prompt_context = Some(PendingPromptContext {
                    text,
                    attached_command_id: None,
                });
            }
        }
        ensure_serialized_budget(
            &next,
            RELAY_SNAPSHOT_BYTE_BUDGET,
            "relay snapshot with pending prompt context",
        )?;
        self.commit_snapshot(next)
    }

    fn attach(
        &mut self,
        after_ordinal: u64,
        after_digest: &str,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        let state = self.operational_state();
        self.replay_plan()
            .attach(after_ordinal, after_digest, state)
    }

    fn acknowledge(
        &mut self,
        through_ordinal: u64,
        through_digest: &str,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        let plan = self.replay_plan();
        if let Err(error) = plan.validate_cursor(through_ordinal, through_digest) {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::Desynchronized,
                error.to_string(),
                false,
                Some(plan.desynchronized_detail(through_ordinal, through_digest)),
            )));
        }
        if through_ordinal > self.snapshot.acknowledged_through {
            let mut next_snapshot = self.snapshot.clone();
            next_snapshot.acknowledged_through = through_ordinal;
            next_snapshot.acknowledged_digest = through_digest.to_owned();
            // The acknowledgement becomes durable before any journal GC.
            self.commit_snapshot(next_snapshot)?;
        }
        // An earlier attempt may have durably advanced the ACK and then
        // failed while rewriting or pruning history. Retrying the exact ACK
        // must retry that cleanup instead of treating it as wholly complete.
        if through_ordinal == self.snapshot.acknowledged_through {
            self.garbage_collect_relay_history()?;
        }
        Ok(Ok(RelayResponsePayload::Acknowledged {
            through_ordinal: self.snapshot.acknowledged_through,
            through_digest: self.snapshot.acknowledged_digest.clone(),
        }))
    }

    fn submit_command(
        &mut self,
        command_id: &str,
        command: RelayCommand,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        if let Err(error) =
            ensure_serialized_budget(&command, RELAY_COMMAND_BYTE_BUDGET, "relay command")
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                error.to_string(),
                false,
                None,
            )));
        }
        if validate_identifier(command_id, "command ID").is_err() {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "invalid command ID",
                false,
                None,
            )));
        }
        if let Some(handled) = self.snapshot.handled_commands.get(command_id) {
            let accepted_ordinal = {
                if handled.command != command {
                    return Ok(Err(relay_protocol_error(
                        RelayErrorCode::InvalidRequest,
                        "command ID was already used for a different command",
                        false,
                        None,
                    )));
                }
                handled.accepted_ordinal
            };
            // A journal append can succeed before snapshot persistence reports
            // an error. Retrying the durable command must resume any remaining
            // relay-local transition instead of merely echoing its first ACK.
            if command.is_relay_local() {
                self.finish_relay_local_command(command_id)?;
            }
            return Ok(Ok(RelayResponsePayload::Accepted {
                command_id: command_id.to_owned(),
                ordinal: accepted_ordinal,
            }));
        }
        if let RelayCommand::Prompt { prompt } = &command {
            let verified = mj_core::attachment::references(prompt).and_then(|references| {
                let store = mj_core::attachment::AttachmentStore::worker(&self.root);
                for reference in references {
                    store.read(&reference)?;
                }
                Ok(())
            });
            if let Err(error) = verified {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidRequest,
                    error.to_string(),
                    false,
                    None,
                )));
            }
        }
        let pending_close_barrier = self.pending_close_barrier_id().map(str::to_owned);
        let completes_pending_close = pending_close_barrier.as_deref().is_some_and(|barrier| {
            matches!(
                &command,
                RelayCommand::CompleteCheckpoint { barrier_command_id }
                    if barrier_command_id == barrier
            )
        });
        if pending_close_barrier.is_some() && !completes_pending_close {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "relay session is sealed for close",
                false,
                None,
            )));
        }
        if matches!(
            self.snapshot.execution,
            RelayExecutionState::Closing | RelayExecutionState::Closed
        ) && !completes_pending_close
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "relay session is closing",
                false,
                None,
            )));
        }
        if let RelayCommand::Prompt { prompt } = &command
            && prompt.is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "prompt is empty",
                false,
                None,
            )));
        }
        if let RelayCommand::RunUserShell { command } = &command
            && command.trim().is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "shell command is empty",
                false,
                None,
            )));
        }
        if let RelayCommand::CancelUserShell { shell_command_id } = &command
            && !self
                .snapshot
                .active_user_shells
                .contains_key(shell_command_id)
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "there is no active shell command with that ID",
                false,
                None,
            )));
        }
        if let RelayCommand::SetConfig { key, value } = &command
            && (key.trim().is_empty() || value.trim().is_empty())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "configuration key and value are required",
                false,
                None,
            )));
        }
        if let RelayCommand::RecordNotice { text } = &command
            && text.trim().is_empty()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "notice text is required",
                false,
                None,
            )));
        }
        // A late cancellation must not advance the checkpoint cursor or leave
        // a cancellation queued for a future turn after the barrier releases.
        if matches!(
            command,
            RelayCommand::CancelTurn | RelayCommand::GoalControl { .. }
        ) && self.snapshot.checkpoint_barrier.is_some()
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "the checkpoint barrier is already admitted",
                false,
                None,
            )));
        }
        if let RelayCommand::Cancel = command
            && self.snapshot.active_prompt.is_none()
            && self
                .snapshot
                .capacity_retry
                .as_ref()
                .is_none_or(|r| r.submitted)
        {
            let message = if self.snapshot.harness_turn.is_some() {
                "the agent is working on its own after a background task; there is no prompt to cancel"
            } else {
                "there is no active prompt to cancel"
            };
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                message,
                false,
                None,
            )));
        }
        if let RelayCommand::RemoveQueuedPrompt { queued_command_id } = &command
            && !self
                .snapshot
                .queued_prompts
                .iter()
                .any(|queued| queued.command_id == *queued_command_id)
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidRequest,
                "unknown queued prompt",
                false,
                None,
            )));
        }
        if let RelayCommand::CompleteCheckpoint { barrier_command_id }
        | RelayCommand::ReleaseCheckpoint { barrier_command_id } = &command
            && (self.snapshot.checkpoint_barrier.as_deref() != Some(barrier_command_id)
                || self.snapshot.checkpoint_ready_through.is_none())
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "checkpoint barrier is not active",
                false,
                None,
            )));
        }
        if let RelayCommand::AdvanceRecoveryFloor { through } = &command
            && let Some(message) = self.recovery_floor_rejection(through)?
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                message,
                false,
                None,
            )));
        }
        if let RelayCommand::Close {
            barrier_command_id,
            expected,
        } = &command
        {
            let ready = self
                .snapshot
                .checkpoint_ready_through
                .zip(self.snapshot.checkpoint_ready_digest.as_ref());
            let exact_cut = self.snapshot.checkpoint_barrier.as_deref() == Some(barrier_command_id)
                && ready.is_some_and(|(ordinal, digest)| {
                    ordinal == expected.ordinal && digest == &expected.digest
                })
                && self.snapshot.latest_ordinal == expected.ordinal
                && self.snapshot.latest_digest == expected.digest;
            if !exact_cut {
                return Ok(Err(relay_protocol_error(
                    RelayErrorCode::InvalidState,
                    "close does not match the current checkpoint cut",
                    false,
                    None,
                )));
            }
        }

        if self.checkpoint_only
            && !command.is_relay_local()
            && !matches!(
                command,
                RelayCommand::BeginCheckpoint { .. } | RelayCommand::Close { .. }
            )
        {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::InvalidState,
                "session is being preserved for Move; resume it to run commands",
                false,
                None,
            )));
        }
        let created_at_ms = epoch_millis();
        let accepted_ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandQueued {
                command_id: command_id.to_owned(),
                command: command.clone(),
                created_at_ms,
            },
        )?;

        if command.is_relay_local() {
            self.finish_relay_local_command(command_id)?;
        }
        self.promote_next_queued_command()?;
        Ok(Ok(RelayResponsePayload::Accepted {
            command_id: command_id.to_owned(),
            ordinal: accepted_ordinal,
        }))
    }

    /// Why a recovery floor move must be refused, or `None` when it is valid.
    ///
    /// Journal garbage collection retains history through this ordinal, so a
    /// cursor off this relay's own event chain would discard events that no
    /// installed archive covers. The floor therefore only moves forward, only
    /// within the durable frontier, and only to a matching digest.
    fn recovery_floor_rejection(&self, through: &RelayCursor) -> Result<Option<String>> {
        if through.ordinal > self.snapshot.latest_ordinal {
            return Ok(Some(format!(
                "recovery floor {} is ahead of the relay frontier {}",
                through.ordinal, self.snapshot.latest_ordinal
            )));
        }
        if through.ordinal < self.snapshot.recovery_floor_ordinal {
            return Ok(Some(format!(
                "recovery floor {} is behind the current floor {}",
                through.ordinal, self.snapshot.recovery_floor_ordinal
            )));
        }
        if validate_relay_digest(&through.digest, "recovery floor digest").is_err() {
            return Ok(Some("recovery floor digest is malformed".to_owned()));
        }
        let Some(expected) = self.digest_at(through.ordinal)? else {
            return Ok(Some(format!(
                "relay digest is unavailable at event {}",
                through.ordinal
            )));
        };
        if through.digest != expected {
            return Ok(Some(format!(
                "recovery floor digest does not match the relay event chain at event {}",
                through.ordinal
            )));
        }
        Ok(None)
    }

    /// Finish a relay-local command from its durable dispatch record. This is
    /// deliberately restartable: every intermediate mutation is an event, so
    /// reopening the relay can resume after any append without duplicating or
    /// skipping the remaining queue transition.
    fn finish_relay_local_command(&mut self, command_id: &str) -> Result<()> {
        let dispatch = self
            .snapshot
            .dispatches
            .get(command_id)
            .with_context(|| format!("unknown relay-local command {command_id}"))?;
        if !dispatch.command.is_relay_local() {
            bail!("command {command_id} is not relay-local");
        }
        let command = dispatch.command.clone();
        let state = dispatch.state;
        if matches!(
            state,
            RelayDispatchState::Completed
                | RelayDispatchState::Rejected
                | RelayDispatchState::Interrupted
        ) {
            if state == RelayDispatchState::Completed && releases_history(&command) {
                // The completion event may be durable even if its following
                // journal GC reported a transient persistence error.
                self.garbage_collect_relay_history()?;
            }
            return Ok(());
        }
        // Recorded before the command starts, and unguarded by dispatch state:
        // a retry that repeats this append is harmless because the projection
        // keys the transcript line on this command, not on the event ordinal.
        if let RelayCommand::RecordNotice { text } = &command {
            let message = text.clone();
            self.append_relay_event(Some(command_id), RelayObservation::Notice { message })?;
        }
        if state == RelayDispatchState::Queued {
            self.append_relay_event(
                Some(command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.to_owned(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }

        let removed_command_ids = match &command {
            RelayCommand::RemoveQueuedPrompt { queued_command_id } => {
                if self
                    .snapshot
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.command_id == *queued_command_id)
                {
                    vec![queued_command_id.clone()]
                } else {
                    Vec::new()
                }
            }
            RelayCommand::ClearQueuedPrompts => self
                .snapshot
                .queued_prompts
                .iter()
                .map(|queued| queued.command_id.clone())
                .collect(),
            _ => Vec::new(),
        };
        let outcome = match &command {
            RelayCommand::CompleteCheckpoint { .. } => RelayCommandOutcome::CheckpointCompleted,
            RelayCommand::ReleaseCheckpoint { .. } => RelayCommandOutcome::CheckpointReleased,
            RelayCommand::AdvanceRecoveryFloor { .. } => RelayCommandOutcome::RecoveryFloorAdvanced,
            RelayCommand::RecordNotice { .. } => RelayCommandOutcome::NoticeRecorded,
            _ => RelayCommandOutcome::QueueChanged {
                removed_command_ids,
            },
        };
        self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                command_id: command_id.to_owned(),
                outcome,
            },
        )?;
        if releases_history(&command) {
            self.garbage_collect_relay_history()?;
        }
        Ok(())
    }

    /// Durably claim commands before they are handed to the live ACP driver.
    /// A checkpoint barrier is admitted only after every previously-started
    /// ACP effect is terminal. Once admitted, it is the sole claimable command
    /// and all ACP dispatch remains frozen until that exact barrier completes.
    pub fn claim_pending_commands(
        &mut self,
        acp_session_configured: bool,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        self.claim_pending_commands_up_to(acp_session_configured, usize::MAX)
    }

    /// Claim no more work than the caller already holds transport permits for.
    /// The dispatcher reserves one ACP command permit per claimed command
    /// before calling, so every claim can be handed over without waiting even
    /// when other senders share that channel. Bounding the durable in-flight
    /// batch that way is what keeps command backpressure from parking the
    /// coordinator that must keep draining ACP's bounded event channel.
    pub fn claim_pending_commands_up_to(
        &mut self,
        acp_session_configured: bool,
        maximum: usize,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        if self.checkpoint_only || !acp_session_configured || maximum == 0 {
            return Ok(Vec::new());
        }
        self.promote_next_queued_command()?;
        if self.snapshot.checkpoint_barrier.is_none() {
            if let Some((barrier_id, barrier_ordinal)) = self.next_queued_checkpoint() {
                let mut earlier_controls = self.queued_controls_before(barrier_ordinal);
                if !earlier_controls.is_empty() {
                    earlier_controls.truncate(maximum);
                    self.start_queued_controls(earlier_controls)?;
                // A turn the harness started on its own is real work in the
                // agent's workspace, so the barrier waits for it exactly as it
                // waits for a prompt.
                } else if !self.effectful_command_in_progress()
                    && self.snapshot.harness_turn.is_none()
                {
                    self.append_relay_event(
                        Some(&barrier_id),
                        RelayObservation::CommandStarted {
                            command_id: barrier_id.clone(),
                            started_at_ms: epoch_millis(),
                        },
                    )?;
                }
            } else {
                let mut controls = self.queued_controls_before(u64::MAX);
                controls.truncate(maximum);
                self.start_queued_controls(controls)?;
            }
        }

        let active_barrier = self.snapshot.checkpoint_barrier.as_deref();
        let mut claimable: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter_map(|(command_id, dispatch)| {
                if dispatch.state != RelayDispatchState::Pending {
                    return None;
                }
                match active_barrier {
                    Some(barrier_id) if command_id == barrier_id => self
                        .snapshot
                        .handled_commands
                        .get(command_id)
                        .map(|handled| (handled.accepted_ordinal, command_id.clone())),
                    Some(_) => None,
                    None => self
                        .snapshot
                        .handled_commands
                        .get(command_id)
                        .map(|handled| (handled.accepted_ordinal, command_id.clone())),
                }
            })
            .collect();
        claimable.sort_by_key(|(accepted_ordinal, _)| *accepted_ordinal);
        claimable.truncate(maximum);
        let mut claimed = Vec::with_capacity(claimable.len());
        let mut next_snapshot = self.snapshot.clone();
        for (accepted_ordinal, command_id) in claimable {
            let steering_prompt = matches!(
                next_snapshot.dispatches[&command_id].command,
                RelayCommand::Cancel
            )
            .then(|| next_snapshot.queued_prompts.first())
            .flatten()
            .and_then(|queued| match &queued.payload {
                StoredQueuedRelayPayload::Prompt { prompt } => Some(ClaimedSteeringPrompt {
                    attachment_root: None,
                    queued_command_id: queued.command_id.clone(),
                    prompt: prompt.clone(),
                }),
                StoredQueuedRelayPayload::SetConfig { .. } => None,
            });
            let dispatch = next_snapshot
                .dispatches
                .get_mut(&command_id)
                .expect("claimable command disappeared");
            dispatch.state = RelayDispatchState::InFlight;
            let hidden_prompt_context = matches!(dispatch.command, RelayCommand::Prompt { .. })
                .then(|| {
                    let mut contexts = Vec::new();
                    if let Some(context) = next_snapshot.pending_prompt_context.as_mut() {
                        if context.attached_command_id.is_none() {
                            context.attached_command_id = Some(command_id.clone());
                        }
                        if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                            contexts.push(context.text.clone());
                        }
                    }
                    for context in &mut next_snapshot.pending_user_shell_contexts {
                        if context.accepted_ordinal >= accepted_ordinal {
                            continue;
                        }
                        if context.attached_command_id.is_none() {
                            context.attached_command_id = Some(command_id.clone());
                        }
                        if context.attached_command_id.as_deref() == Some(command_id.as_str()) {
                            contexts.push(context.text.clone());
                        }
                    }
                    (!contexts.is_empty()).then(|| contexts.join("\n\n"))
                })
                .flatten();
            claimed.push(ClaimedRelayCommand {
                command_id,
                accepted_ordinal,
                command: dispatch.command.clone(),
                hidden_prompt_context,
                steering_prompt,
            });
        }
        if !claimed.is_empty() {
            // An in-flight claim is not in the journal, so it is only durable
            // once the snapshot itself is.
            self.commit_snapshot(next_snapshot)?;
            if claimed
                .iter()
                .any(|claim| matches!(claim.command, RelayCommand::Prompt { .. }))
            {
                self.capacity_response = CapacityResponse::default();
            }
        }
        Ok(claimed)
    }

    /// Advance only lifecycle commands after the old owning process was stopped.
    /// No ACP channel or harness readiness is involved in this mode.
    pub fn dispatch_checkpoint_only(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.checkpoint_only,
            "worker is not in checkpoint-only mode"
        );
        let mut commands: Vec<_> = self
            .snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                matches!(
                    dispatch.command,
                    RelayCommand::BeginCheckpoint { .. } | RelayCommand::Close { .. }
                )
            })
            .filter(|(_, dispatch)| {
                matches!(
                    dispatch.state,
                    RelayDispatchState::Queued | RelayDispatchState::Pending
                )
            })
            .map(|(id, _)| {
                (
                    self.snapshot.handled_commands[id].accepted_ordinal,
                    id.clone(),
                )
            })
            .collect();
        commands.sort();
        for (_, id) in commands {
            let command = self.snapshot.dispatches[&id].command.clone();
            // Close is sealed by acceptance but executes only after the controller
            // releases its verified barrier, just like the ordinary coordinator.
            if self.snapshot.checkpoint_barrier.is_some() {
                continue;
            }
            if self.snapshot.dispatches[&id].state == RelayDispatchState::Queued {
                self.append_relay_event(
                    Some(&id),
                    RelayObservation::CommandStarted {
                        command_id: id.clone(),
                        started_at_ms: epoch_millis(),
                    },
                )?;
            }
            let mut next = self.snapshot.clone();
            next.dispatches
                .get_mut(&id)
                .expect("lifecycle command")
                .state = RelayDispatchState::InFlight;
            self.commit_snapshot(next)?;
            match command {
                RelayCommand::BeginCheckpoint { .. } => {
                    self.record_checkpoint_ready(&id)?;
                }
                RelayCommand::Close { .. } => {
                    self.record_command_completed(&id, RelayCommandOutcome::Closed)?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    /// Claim user shell work independently of ACP turns. Run commands honor
    /// the caller's concurrency limit; cancellation controls bypass it so a
    /// full shell pool can always be stopped.
    pub fn claim_pending_user_shell_commands_up_to(
        &mut self,
        maximum_runs: usize,
    ) -> Result<Vec<ClaimedRelayCommand>> {
        if self.checkpoint_only || self.snapshot.checkpoint_barrier.is_some() {
            return Ok(Vec::new());
        }
        let barrier_ordinal = self
            .next_queued_checkpoint()
            .map_or(u64::MAX, |(_, ordinal)| ordinal);
        let mut cancels = Vec::new();
        let cancelled_shells: std::collections::BTreeSet<String> = self
            .snapshot
            .dispatches
            .values()
            .filter(|dispatch| dispatch.state == RelayDispatchState::Queued)
            .filter_map(|dispatch| match &dispatch.command {
                RelayCommand::CancelUserShell { shell_command_id } => {
                    Some(shell_command_id.clone())
                }
                _ => None,
            })
            .collect();
        let mut runs = Vec::new();
        for (command_id, dispatch) in &self.snapshot.dispatches {
            if dispatch.state != RelayDispatchState::Queued {
                continue;
            }
            let Some(handled) = self.snapshot.handled_commands.get(command_id) else {
                continue;
            };
            match dispatch.command {
                RelayCommand::CancelUserShell { .. } => {
                    cancels.push((handled.accepted_ordinal, command_id.clone()));
                }
                RelayCommand::RunUserShell { .. }
                    if handled.accepted_ordinal < barrier_ordinal
                        && !cancelled_shells.contains(command_id) =>
                {
                    runs.push((handled.accepted_ordinal, command_id.clone()));
                }
                _ => {}
            }
        }
        cancels.sort();
        runs.sort();
        runs.truncate(maximum_runs);
        let mut selected = cancels;
        selected.extend(runs);
        selected.sort();
        for (_, command_id) in &selected {
            self.append_relay_event(
                Some(command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.clone(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        let mut next_snapshot = self.snapshot.clone();
        let mut claimed = Vec::with_capacity(selected.len());
        for (accepted_ordinal, command_id) in selected {
            let dispatch = next_snapshot
                .dispatches
                .get_mut(&command_id)
                .expect("claimed shell command disappeared");
            dispatch.state = RelayDispatchState::InFlight;
            claimed.push(ClaimedRelayCommand {
                command_id,
                accepted_ordinal,
                command: dispatch.command.clone(),
                hidden_prompt_context: None,
                steering_prompt: None,
            });
        }
        if !claimed.is_empty() {
            self.commit_snapshot(next_snapshot)?;
        }
        Ok(claimed)
    }

    fn start_queued_controls(&mut self, command_ids: Vec<String>) -> Result<()> {
        for command_id in command_ids {
            self.append_relay_event(
                Some(&command_id),
                RelayObservation::CommandStarted {
                    command_id: command_id.clone(),
                    started_at_ms: epoch_millis(),
                },
            )?;
        }
        Ok(())
    }

    fn queued_controls_before(&self, before_ordinal: u64) -> Vec<String> {
        let active_prompt_ordinal = self.snapshot.active_prompt.as_ref().and_then(|active| {
            self.snapshot
                .handled_commands
                .get(&active.command_id)
                .map(|handled| handled.accepted_ordinal)
        });
        let mut controls: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter_map(|(command_id, dispatch)| {
                if dispatch.state != RelayDispatchState::Queued
                    || !dispatch.command.is_effectful_acp()
                    || dispatch.command.is_queue_entry()
                {
                    return None;
                }
                let accepted = self
                    .snapshot
                    .handled_commands
                    .get(command_id)?
                    .accepted_ordinal;
                // Preserve controls accepted before the active prompt (they
                // must reach ACP first), but keep later controls queued until
                // that prompt finishes. The legacy Cancel control deliberately
                // bypasses a running prompt and may carry steering; CancelTurn
                // bypasses a pending checkpoint as well, but never steers.
                if active_prompt_ordinal.is_some_and(|prompt| accepted > prompt)
                    && !matches!(
                        dispatch.command,
                        RelayCommand::Cancel
                            | RelayCommand::CancelTurn
                            | RelayCommand::GoalControl { .. }
                    )
                {
                    return None;
                }
                let running_turn =
                    self.snapshot.active_prompt.is_some() || self.snapshot.harness_turn.is_some();
                if accepted < before_ordinal
                    || (running_turn && matches!(dispatch.command, RelayCommand::CancelTurn))
                    || matches!(dispatch.command, RelayCommand::GoalControl { .. })
                {
                    Some((accepted, command_id.clone()))
                } else {
                    None
                }
            })
            .collect();
        controls.sort_by_key(|(ordinal, _)| *ordinal);
        controls
            .into_iter()
            .map(|(_, command_id)| command_id)
            .collect()
    }

    fn effectful_command_in_progress(&self) -> bool {
        self.snapshot.active_prompt.is_some()
            || self.snapshot.dispatches.values().any(|dispatch| {
                (dispatch.command.is_effectful_acp() || dispatch.command.is_effectful_user_shell())
                    && matches!(
                        dispatch.state,
                        RelayDispatchState::Pending | RelayDispatchState::InFlight
                    )
            })
    }

    fn next_queued_checkpoint(&self) -> Option<(String, u64)> {
        self.snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                dispatch.state == RelayDispatchState::Queued
                    && matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
            })
            .filter_map(|(command_id, _)| {
                self.snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| (command_id.clone(), handled.accepted_ordinal))
            })
            .min_by_key(|(_, accepted)| *accepted)
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
        if self.background_work == BackgroundWorkPolicy::CodexExecCards
            && self.snapshot.active_prompt.is_some()
        {
            self.capacity_response.observe(&update);
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
        // else: no model turn runs, so no origin marker would ever settle a
        // turn opened for it. Consume the expectation and keep the line.
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
        let settles = claude.then(|| claude_turn_origin(&update)).flatten();
        self.track_foreground_tool(&update);
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
        // Any origin kind settles the turn: the marker means the SDK reached a
        // turn boundary, whatever started the work.
        if let Some(origin) = settles.or_else(|| {
            (codex && native_before && !native_after.running()).then_some(Some("codex".into()))
        }) && self.snapshot.harness_turn.is_some()
        {
            self.append_relay_event(
                None,
                RelayObservation::HarnessTurnSettled {
                    origin,
                    // The projection cannot see `active_prompt`, so the event
                    // has to carry whether a prompt of ours is still running.
                    prompt_in_flight: self.snapshot.active_prompt.is_some(),
                },
            )?;
            self.finish_turn_activity()?;
        }
        Ok(ordinal)
    }

    /// Forget provisional tool statuses at a boundary the harness itself has
    /// confirmed. Independently tracked background work survives into idle.
    fn finish_turn_activity(&mut self) -> Result<()> {
        self.foreground_tools.clear();
        self.codex_execute_tools
            .retain(|tool_call_id, _| self.background_exec_cards.contains_key(tool_call_id));
        self.persist_activity_transition()
    }

    /// Track tool statuses that prove the agent is still doing foreground
    /// work. Pending and in-progress have portable ACP meanings across every
    /// harness; prose and plan updates do not carry a corresponding end, so
    /// they cannot safely override known background work on their own.
    fn track_foreground_tool(&mut self, update: &SessionUpdate) {
        let (tool_call_id, status) = match update {
            SessionUpdate::ToolCall(call) => (call.tool_call_id.0.as_ref(), Some(call.status)),
            SessionUpdate::ToolCallUpdate(call) => {
                (call.tool_call_id.0.as_ref(), call.fields.status)
            }
            _ => return,
        };
        let Some(status) = status else {
            return;
        };
        if matches!(
            status,
            agent_client_protocol::schema::v1::ToolCallStatus::Pending
                | agent_client_protocol::schema::v1::ToolCallStatus::InProgress
        ) {
            let started_at_ms = self.step_clock.started_at_ms().unwrap_or_else(epoch_millis);
            self.foreground_tools
                .entry(tool_call_id.to_owned())
                .and_modify(|(previous, previous_started_at_ms)| {
                    if *previous != status {
                        *previous = status;
                        *previous_started_at_ms = started_at_ms;
                    }
                })
                .or_insert((status, started_at_ms));
        } else {
            self.foreground_tools.remove(tool_call_id);
        }
    }

    /// Follow a Codex `exec_command` card: a result with a null exit code is a
    /// process Codex left running, and the next card for the same call reports
    /// the exit that ends it.
    ///
    /// Codex runs its own shells and never asks Hel for a terminal, so a card
    /// is the only evidence there is. The card shape is the one described in
    /// `.agents/docs/claude-autonomous-turns.md`: an execute card whose
    /// `rawOutput` object carries `exit_code`, null while the process runs.
    fn track_codex_exec_card(&mut self, update: &SessionUpdate) {
        let (tool_call_id, kind, status, raw_input, raw_output) = match update {
            SessionUpdate::ToolCall(call) => (
                call.tool_call_id.0.as_ref(),
                Some(call.kind),
                Some(call.status),
                call.raw_input.as_ref(),
                call.raw_output.as_ref(),
            ),
            SessionUpdate::ToolCallUpdate(call) => (
                call.tool_call_id.0.as_ref(),
                call.fields.kind,
                call.fields.status,
                call.fields.raw_input.as_ref(),
                call.fields.raw_output.as_ref(),
            ),
            _ => return,
        };
        match kind {
            Some(agent_client_protocol::schema::v1::ToolKind::Execute) => {
                let command =
                    exec_card_command(raw_input).unwrap_or_else(|| tool_call_id.to_owned());
                self.codex_execute_tools
                    .insert(tool_call_id.to_owned(), command);
            }
            Some(_) => {
                self.codex_execute_tools.remove(tool_call_id);
                self.background_exec_cards.remove(tool_call_id);
                return;
            }
            None => {}
        }
        let Some(command) = self.codex_execute_tools.get(tool_call_id).cloned() else {
            // Tool-call updates are partial. A result with no kind belongs to
            // the card that introduced it; it is not evidence of execution by
            // itself (guardian reviews and searches also carry raw output).
            return;
        };
        if status.is_some_and(|status| {
            status == agent_client_protocol::schema::v1::ToolCallStatus::Failed
        }) {
            self.codex_execute_tools.remove(tool_call_id);
            self.background_exec_cards.remove(tool_call_id);
            return;
        }
        // A card with no result says nothing about a process: an execute card
        // is in flight until Codex reports its output.
        let Some(raw_output) = raw_output else {
            return;
        };
        // MCP calls also have kind Execute, but their result/error envelope
        // has no process exit field. Absence is not evidence of detachment.
        let Some(exit_code) = raw_output.get("exit_code") else {
            return;
        };
        if !exit_code.is_null() {
            self.codex_execute_tools.remove(tool_call_id);
            self.background_exec_cards.remove(tool_call_id);
            return;
        }
        self.background_exec_cards
            .entry(tool_call_id.to_owned())
            .or_insert(BackgroundCommand {
                id: background_task_id("codex", tool_call_id),
                started_at_ms: epoch_millis(),
                command,
                can_stop: false,
            });
    }

    /// Remember which hosted terminal a tool call embedded. Kimi launches a
    /// detached shell through a terminal of ours and reports the terminal id
    /// on the launcher card, which is the only link between the ACP card and
    /// the process its native journal later names.
    fn track_agent_terminal_tool_call(&mut self, update: &SessionUpdate) {
        use agent_client_protocol::schema::v1::ToolCallContent;

        let (tool_call_id, content) = match update {
            SessionUpdate::ToolCall(call) => (call.tool_call_id.0.as_ref(), Some(&call.content)),
            SessionUpdate::ToolCallUpdate(call) => {
                (call.tool_call_id.0.as_ref(), call.fields.content.as_ref())
            }
            _ => return,
        };
        let Some(content) = content else {
            return;
        };
        for item in content {
            if let ToolCallContent::Terminal(terminal) = item {
                self.agent_terminal_tool_calls.insert(
                    kimi_native_tool_call_id(tool_call_id).to_owned(),
                    terminal.terminal_id.0.to_string(),
                );
            }
        }
    }

    /// Keep positive ACP evidence until Kimi's native journal proves which
    /// detached task the launcher created. Kimi completes the launcher card
    /// immediately, so a completed status is not a child-liveness boundary.
    fn track_kimi_background_agent(&mut self, update: &SessionUpdate) {
        let (tool_call_id, title, status, raw_input, raw_output) = match update {
            SessionUpdate::ToolCall(call) => (
                call.tool_call_id.0.as_ref(),
                Some(call.title.as_str()),
                Some(call.status),
                call.raw_input.as_ref(),
                call.raw_output.as_ref(),
            ),
            SessionUpdate::ToolCallUpdate(call) => (
                call.tool_call_id.0.as_ref(),
                call.fields.title.as_deref(),
                call.fields.status,
                call.fields.raw_input.as_ref(),
                call.fields.raw_output.as_ref(),
            ),
            _ => return,
        };
        if status == Some(agent_client_protocol::schema::v1::ToolCallStatus::Failed) {
            self.kimi_provisional_tasks.remove(tool_call_id);
            return;
        }
        let task_id = kimi_running_task_id(raw_output);
        let explicit_background = raw_input
            .and_then(|input| input.get("run_in_background"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let background_title =
            title.is_some_and(|title| title.starts_with("Launching background "));
        if !explicit_background && !background_title && task_id.is_none() {
            return;
        }
        if self
            .kimi_observed_tool_ids
            .contains(kimi_native_tool_call_id(tool_call_id))
            || task_id.is_some_and(|id| self.kimi_observed_task_ids.contains(id))
        {
            self.kimi_provisional_tasks.remove(tool_call_id);
            return;
        }
        // Several TaskOutput/WaitFor calls may observe the same native task.
        // Keep one provisional entry until its lifecycle stream catches up.
        let mut started_at_ms = self.step_clock.started_at_ms().unwrap_or_else(epoch_millis);
        if let Some(id) = task_id {
            self.kimi_provisional_tasks.retain(|existing_call, task| {
                if task.native_task_id.as_deref() == Some(id) {
                    started_at_ms = started_at_ms.min(task.command.started_at_ms);
                    existing_call == tool_call_id
                } else {
                    true
                }
            });
        }
        let command = kimi_agent_description(title, raw_input, task_id);
        self.kimi_provisional_tasks
            .entry(tool_call_id.to_owned())
            .and_modify(|task| {
                task.command.command = command.clone();
                if let Some(id) = task_id {
                    task.native_task_id = Some(id.to_owned());
                }
            })
            .or_insert(KimiProvisionalTask {
                command: BackgroundCommand {
                    id: background_task_id("kimi-provisional", tool_call_id),
                    started_at_ms,
                    command,
                    can_stop: false,
                },
                native_task_id: task_id.map(str::to_owned),
            });
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

    pub fn capacity_retry_deadline(&self) -> Option<i64> {
        self.snapshot
            .capacity_retry
            .as_ref()
            .filter(|r| !r.submitted)
            .map(|r| r.retry_at_ms)
    }

    /// Admit a due retry through the same durable queue as an external prompt.
    pub fn submit_due_capacity_retry(&mut self, now_ms: i64) -> Result<bool> {
        let Some(retry) = self
            .snapshot
            .capacity_retry
            .as_ref()
            .filter(|r| !r.submitted)
        else {
            return Ok(false);
        };
        if retry.retry_at_ms > now_ms
            || self.background_work != BackgroundWorkPolicy::CodexExecCards
            || !self.acp_ready
            || self.snapshot.execution != RelayExecutionState::Idle
            || self.snapshot.active_prompt.is_some()
            || self.snapshot.harness_turn.is_some()
            || !self.snapshot.queued_prompts.is_empty()
            || self.snapshot.checkpoint_barrier.is_some()
            || self.pending_close_barrier_id().is_some()
        {
            return Ok(false);
        }
        let id = retry.command_id.clone();
        let prompt = vec![ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new("Continue"),
        )];
        self.submit_command(&id, RelayCommand::Prompt { prompt })?
            .map_err(|error| anyhow!("submit capacity retry: {error:?}"))?;
        Ok(true)
    }

    pub fn record_command_completed(
        &mut self,
        command_id: &str,
        outcome: RelayCommandOutcome,
    ) -> Result<u64> {
        self.require_in_flight(command_id)?;
        if matches!(
            self.snapshot.dispatches[command_id].command,
            RelayCommand::BeginCheckpoint { .. }
        ) {
            bail!("checkpoint barriers complete through record_checkpoint_ready");
        }
        let mut outcome = outcome;
        if let RelayCommandOutcome::Prompt { stop_reason, .. } = &mut outcome {
            if self.background_work == BackgroundWorkPolicy::CodexExecCards
                && matches!(
                    stop_reason.as_str(),
                    "EndTurn" | "end_turn" | "Error" | "error"
                )
                && self.capacity_response.at_capacity()
            {
                *stop_reason = CAPACITY_STOP_REASON.to_owned();
            }
            self.capacity_response = CapacityResponse::default();
        }
        let finishes_turn = matches!(outcome, RelayCommandOutcome::Prompt { .. });
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandCompleted {
                command_id: command_id.to_owned(),
                outcome,
            },
        )?;
        if finishes_turn && !self.snapshot.goal.running() {
            self.finish_turn_activity()?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_rejected(
        &mut self,
        command_id: &str,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandRejected {
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_command_interrupted(
        &mut self,
        command_id: &str,
        message: impl Into<String>,
    ) -> Result<u64> {
        self.require_dispatch(command_id)?;
        let command = self.snapshot.dispatches[command_id].command.kind();
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandInterrupted {
                command_id: command_id.to_owned(),
                command,
                message: message.into(),
            },
        )?;
        if command == RelayCommandKind::Prompt {
            self.finish_turn_activity()?;
        }
        self.promote_next_queued_command()?;
        Ok(ordinal)
    }

    pub fn record_checkpoint_ready(&mut self, command_id: &str) -> Result<u64> {
        self.require_in_flight(command_id)?;
        if self.snapshot.checkpoint_barrier.as_deref() != Some(command_id) {
            bail!("checkpoint barrier {command_id} is not active");
        }
        if self.snapshot.checkpoint_ready_through.is_some() {
            bail!("checkpoint barrier {command_id} is already ready");
        }
        if !matches!(
            self.snapshot.dispatches[command_id].command,
            RelayCommand::BeginCheckpoint { .. }
        ) {
            bail!("command {command_id} is not a checkpoint barrier");
        }
        let through = self
            .snapshot
            .latest_ordinal
            .checked_add(1)
            .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
        self.append_relay_event(
            Some(command_id),
            RelayObservation::CheckpointReady {
                command_id: command_id.to_owned(),
                through,
            },
        )
    }

    /// Release checkpoint barriers owned by a controller connection that
    /// disappeared. The runtime calls this when that connection drops so an
    /// offline prompt queue can never remain paused indefinitely.
    pub fn cancel_checkpoint_barrier_on_disconnect(
        &mut self,
        command_id: &str,
    ) -> Result<Option<u64>> {
        let Some(dispatch) = self.snapshot.dispatches.get(command_id) else {
            return Ok(None);
        };
        if !matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
            || !matches!(
                dispatch.state,
                RelayDispatchState::Queued
                    | RelayDispatchState::Pending
                    | RelayDispatchState::InFlight
            )
        {
            return Ok(None);
        }
        let ordinal = self.append_relay_event(
            Some(command_id),
            RelayObservation::CommandInterrupted {
                command_id: command_id.to_owned(),
                command: RelayCommandKind::BeginCheckpoint,
                message: "checkpoint barrier cancelled because its controller disconnected"
                    .to_owned(),
            },
        )?;
        self.promote_next_queued_command()?;
        Ok(Some(ordinal))
    }

    fn require_dispatch(&self, command_id: &str) -> Result<()> {
        if !self.snapshot.dispatches.contains_key(command_id) {
            bail!("unknown relay command {command_id}");
        }
        Ok(())
    }

    fn require_in_flight(&self, command_id: &str) -> Result<()> {
        let Some(dispatch) = self.snapshot.dispatches.get(command_id) else {
            bail!("unknown relay command {command_id}");
        };
        if dispatch.state != RelayDispatchState::InFlight {
            bail!("relay command {command_id} is not in flight");
        }
        Ok(())
    }

    fn close_requested(&self) -> bool {
        self.pending_close_barrier_id().is_some()
    }

    fn pending_close_barrier_id(&self) -> Option<&str> {
        self.snapshot.dispatches.values().find_map(|dispatch| {
            if matches!(
                dispatch.state,
                RelayDispatchState::Completed
                    | RelayDispatchState::Rejected
                    | RelayDispatchState::Interrupted
            ) {
                return None;
            }
            match &dispatch.command {
                RelayCommand::Close {
                    barrier_command_id, ..
                } => Some(barrier_command_id.as_str()),
                _ => None,
            }
        })
    }

    fn digest_at(&self, ordinal: u64) -> Result<Option<String>> {
        self.replay_plan().digest_at(ordinal)
    }

    /// Start the head of the durable command queue once the relay is idle.
    /// Entries run strictly one at a time, in the order they were accepted.
    fn promote_next_queued_command(&mut self) -> Result<Option<u64>> {
        // A turn the harness started on its own leaves execution Running, but
        // it must not hold a queued prompt: the adapter queues a prompt that
        // arrives mid-turn and answers it as soon as that turn ends.
        // `active_prompt` is the real gate on dispatch.
        if self.checkpoint_only
            || self.snapshot.active_prompt.is_some()
            || self.promoted_config_in_progress()
            || self.snapshot.checkpoint_barrier.is_some()
            || matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
            || self.pending_checkpoint_barrier()
            || self.close_requested()
        {
            return Ok(None);
        }
        let Some(queued) = self.snapshot.queued_prompts.first().cloned() else {
            return Ok(None);
        };
        let queued_ordinal = self
            .snapshot
            .handled_commands
            .get(&queued.command_id)
            .map_or(u64::MAX, |handled| handled.accepted_ordinal);
        if self.snapshot.active_user_shells.keys().any(|command_id| {
            self.snapshot
                .handled_commands
                .get(command_id)
                .is_some_and(|handled| handled.accepted_ordinal < queued_ordinal)
        }) {
            return Ok(None);
        }
        let ordinal = self.append_relay_event(
            Some(&queued.command_id),
            RelayObservation::CommandStarted {
                command_id: queued.command_id.clone(),
                started_at_ms: epoch_millis(),
            },
        )?;
        Ok(Some(ordinal))
    }

    /// A promoted configuration change leaves execution idle while it reaches
    /// ACP, so the queue needs its own guard to stay sequential. Completion,
    /// rejection, and interruption all promote the next entry.
    fn promoted_config_in_progress(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::SetConfig { .. })
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Pending | RelayDispatchState::InFlight
                )
        })
    }

    fn pending_checkpoint_barrier(&self) -> bool {
        self.snapshot.dispatches.values().any(|dispatch| {
            matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
                && matches!(
                    dispatch.state,
                    RelayDispatchState::Queued
                        | RelayDispatchState::Pending
                        | RelayDispatchState::InFlight
                )
        })
    }
}

struct RelayEventPage {
    events: Vec<RelayEvent>,
    through_ordinal: u64,
    through_digest: String,
}

/// A point-in-time view of the durable journal: everything needed to validate
/// a replay cursor and assemble a replay page, and nothing that requires the
/// relay lock to read.
///
/// Sealed segments are immutable and the active segment is append-only, so a
/// captured span list stays readable while the relay keeps recording events.
/// Sealing and garbage collection do invalidate it, so every read here is
/// written to fail loudly rather than return a short or torn page, and
/// `generation` lets the caller recognize that failure as a stale plan instead
/// of a real desynchronization.
pub struct RelayReplayPlan {
    spans: Vec<RelayJournalSpan>,
    /// Digests of recent live events and proven replay-page cursors, so the
    /// next sequential attachment validates without touching old segments.
    hot_digests: Vec<(u64, String)>,
    latest_ordinal: u64,
    latest_digest: String,
    acknowledged_through: u64,
    acknowledged_digest: String,
    recovery_floor_ordinal: u64,
    recovery_floor_digest: String,
    retained_through: u64,
    retained_digest: String,
    generation: u64,
}

/// A journal span could not be parsed during a replay. When newer readable
/// history exists past it, `read_events_after` attaches this to the error so
/// `attach` can answer with a `Desynchronized` recovery cursor rather than a
/// retryable failure that the controller would loop on forever.
#[derive(Debug)]
struct UnreadableRelaySpan {
    recover_after: u64,
}

impl std::fmt::Display for UnreadableRelaySpan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "relay history is unreadable; readable events resume after event {}",
            self.recover_after
        )
    }
}

impl std::error::Error for UnreadableRelaySpan {}

impl RelayReplayPlan {
    fn attach(
        &self,
        after_ordinal: u64,
        after_digest: &str,
        state: RelayOperationalState,
    ) -> Result<std::result::Result<RelayResponsePayload, RelayProtocolError>> {
        if let Err(error) = self.validate_cursor(after_ordinal, after_digest) {
            return Ok(Err(relay_protocol_error(
                RelayErrorCode::Desynchronized,
                error.to_string(),
                false,
                Some(self.desynchronized_detail(after_ordinal, after_digest)),
            )));
        }
        let page =
            match self.read_events_after(after_ordinal, after_digest, RELAY_REPLAY_BYTE_BUDGET) {
                Ok(page) => page,
                Err(error) => {
                    // An unreadable old span cannot be served, but newer history
                    // still can. Answer with a desynchronization cursor past the
                    // corruption so the controller resynchronizes from there
                    // instead of retrying the same unparseable bytes forever.
                    if let Some(gap) = error.downcast_ref::<UnreadableRelaySpan>() {
                        let (earliest_available, earliest_digest) =
                            self.recovery_cursor_after(gap.recover_after);
                        return Ok(Err(relay_protocol_error(
                            RelayErrorCode::Desynchronized,
                            error.to_string(),
                            false,
                            Some(RelayErrorDetail::Desynchronized {
                                requested_after: after_ordinal,
                                requested_digest: after_digest.to_owned(),
                                earliest_available,
                                earliest_digest,
                                latest: self.latest_ordinal,
                                latest_digest: self.latest_digest.clone(),
                            }),
                        )));
                    }
                    return Err(error);
                }
            };
        ensure_serialized_budget(&state, RELAY_STATE_BYTE_BUDGET, "relay operational state")?;
        Ok(Ok(RelayResponsePayload::Attached {
            state,
            events: page.events,
            through_ordinal: page.through_ordinal,
            through_digest: page.through_digest,
        }))
    }

    /// A resync cursor the controller can attach at to recover history past an
    /// unreadable span. It must name an ordinal whose digest is resolvable
    /// *without* the corrupt span, so it points at the first readable, self-
    /// valid event strictly after the corruption. The controller re-attaches
    /// after it and recovers every later event; at most the corrupt span and
    /// that one boundary event are lost. When nothing readable remains, it
    /// resumes live at the frontier.
    fn recovery_cursor_after(&self, corrupt_last_ordinal: u64) -> (u64, String) {
        for span in &self.spans {
            if span.file_last_ordinal <= corrupt_last_ordinal {
                continue;
            }
            let mut found = None;
            let _ = visit_relay_journal_file(&span.path, JournalReadMode::Recover, |event, _| {
                if event.ordinal > corrupt_last_ordinal && validate_relay_event_self(&event).is_ok()
                {
                    found = Some((event.ordinal, event.digest));
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            });
            if let Some(cursor) = found {
                return cursor;
            }
        }
        (self.latest_ordinal, self.latest_digest.clone())
    }

    fn validate_cursor(&self, after_ordinal: u64, after_digest: &str) -> Result<()> {
        if after_ordinal < self.retained_through {
            bail!(
                "event {after_ordinal} is no longer available; relay retained events after {}",
                self.retained_through
            );
        }
        if after_ordinal > self.latest_ordinal {
            bail!(
                "event {after_ordinal} is newer than relay frontier {}",
                self.latest_ordinal
            );
        }
        validate_relay_digest(after_digest, "event cursor digest")?;
        let expected = self
            .digest_at(after_ordinal)?
            .ok_or_else(|| anyhow!("relay digest missing at event {after_ordinal}"))?;
        if after_digest != expected {
            bail!("event {after_ordinal} digest does not match the relay event chain");
        }
        Ok(())
    }

    fn digest_at(&self, ordinal: u64) -> Result<Option<String>> {
        if ordinal == 0 {
            return Ok(Some(RELAY_EVENT_GENESIS_DIGEST.to_owned()));
        }
        if ordinal == self.latest_ordinal {
            return Ok(Some(self.latest_digest.clone()));
        }
        if ordinal == self.acknowledged_through {
            return Ok(Some(self.acknowledged_digest.clone()));
        }
        if ordinal == self.recovery_floor_ordinal {
            return Ok(Some(self.recovery_floor_digest.clone()));
        }
        if let Some((_, digest)) = self.hot_digests.iter().find(|(hot, _)| *hot == ordinal) {
            return Ok(Some(digest.clone()));
        }
        // A v1 span caches the digest at its boundary in the first event's
        // `previous_digest`, so the digest before the span can be read without
        // touching the segment. v2 records carry no such back-reference
        // (`file_first_previous_digest` is None), so fall through to read the
        // event at `ordinal` directly.
        if let Some(span) = self.spans.iter().find(|span| {
            span.file_first_ordinal.checked_sub(1) == Some(ordinal) && ordinal >= span.after_ordinal
        }) {
            if let Some(digest) = &span.file_first_previous_digest {
                return Ok(Some(digest.clone()));
            }
            let mut digest = None;
            visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
                validate_relay_event_self(&event)
                    .with_context(|| format!("validate relay journal {}", span.path.display()))?;
                if event.format == RELAY_EVENT_FORMAT_V1 && !event.previous_digest.is_empty() {
                    digest = Some(event.previous_digest);
                }
                Ok(ControlFlow::Break(()))
            })?;
            if digest.is_some() {
                return Ok(digest);
            }
            // v2 first event: no cached boundary digest; read the target itself.
        }
        let Some(span) = self
            .spans
            .iter()
            .find(|span| ordinal > span.after_ordinal && ordinal <= span.file_last_ordinal)
        else {
            return Ok(None);
        };
        let mut digest = None;
        let mut previous: Option<RelayEvent> = None;
        visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
            // Each record is validated by its own digest (the corruption check).
            // Between consecutive records the v1 chain link is also enforced;
            // v2 records have no link and are trusted on their own digest.
            validate_relay_event_self(&event)
                .with_context(|| format!("validate relay journal {}", span.path.display()))?;
            if let Some(previous) = &previous
                && event.format == RELAY_EVENT_FORMAT_V1
                && event.previous_digest != previous.digest
            {
                bail!(
                    "relay journal {} event {} does not chain from event {}",
                    span.path.display(),
                    event.ordinal,
                    previous.ordinal
                );
            }
            if event.ordinal == ordinal {
                digest = Some(event.digest.clone());
                return Ok(ControlFlow::Break(()));
            }
            previous = Some(event);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(digest)
    }

    fn read_events_after(
        &self,
        after_ordinal: u64,
        after_digest: &str,
        byte_budget: usize,
    ) -> Result<RelayEventPage> {
        let mut events = Vec::new();
        let mut used = 0_usize;
        let mut through_ordinal = after_ordinal;
        // `attach` validated this exact cursor immediately before entering the
        // reader. Reusing it avoids a second decompression pass over the
        // cursor's sealed segment.
        let mut through_digest = after_digest.to_owned();
        let mut page_full = false;

        for span in &self.spans {
            if page_full || span.file_last_ordinal <= through_ordinal {
                continue;
            }
            let read = visit_relay_journal_file(
                &span.path,
                JournalReadMode::Strict,
                |event, encoded_len| {
                    if event.ordinal <= span.after_ordinal || event.ordinal <= through_ordinal {
                        return Ok(ControlFlow::Continue(()));
                    }
                    if event.ordinal > self.latest_ordinal {
                        // The active segment kept growing after this plan was
                        // captured. Those events are real, but the reply's
                        // operational state describes the frontier the plan saw,
                        // so the page stops there and the caller asks again.
                        return Ok(ControlFlow::Break(()));
                    }
                    let expected = through_ordinal
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
                    if event.ordinal != expected {
                        bail!(
                            "relay journal page has a gap after event {through_ordinal}: found {}",
                            event.ordinal
                        );
                    }
                    // The page is assembled off the relay lock, so it carries its
                    // own proof that it is one unbroken run the cursor named rather
                    // than fragments of a journal that moved. For v1 the in-record
                    // chain link proves this; a v2 page relies instead on both
                    // endpoints being digest anchors (the cursor and the frontier)
                    // plus each interior record self-validating and the ordinals
                    // being contiguous.
                    if event.format == RELAY_EVENT_FORMAT_V1
                        && event.previous_digest != through_digest
                    {
                        bail!(
                            "relay journal event {} does not chain from event {through_ordinal}",
                            event.ordinal
                        );
                    }
                    validate_relay_event(through_ordinal, &through_digest, &event)
                        .context("validate relay journal page event")?;
                    if !events.is_empty() && used.saturating_add(encoded_len) > byte_budget {
                        page_full = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    used = used.saturating_add(encoded_len);
                    through_ordinal = event.ordinal;
                    through_digest.clone_from(&event.digest);
                    events.push(event);
                    Ok(ControlFlow::Continue(()))
                },
            );
            if let Err(error) = read {
                // This span will not parse. If newer, readable history exists
                // past it, mark the error so `attach` can send the controller a
                // recovery cursor after this span instead of a retryable failure
                // it would loop on. The corrupt bytes are never served as valid.
                if span.file_last_ordinal < self.latest_ordinal {
                    return Err(error.context(UnreadableRelaySpan {
                        recover_after: span.file_last_ordinal,
                    }));
                }
                return Err(error);
            }
            // A canonical span contributes every ordinal through its last one.
            // Stopping short means the file no longer holds what this plan
            // captured: it was sealed, rewritten, or pruned under the reader.
            if !page_full && through_ordinal < span.file_last_ordinal.min(self.latest_ordinal) {
                bail!(
                    "relay journal {} no longer covers event {}",
                    span.path.display(),
                    span.file_last_ordinal
                );
            }
        }
        if !page_full
            && (through_ordinal != self.latest_ordinal || through_digest != self.latest_digest)
        {
            // The spans end at the frontier this plan captured. Anything else
            // is a page assembled from a journal that moved, never a short
            // answer a caller could mistake for a complete one.
            bail!(
                "relay journal ended at event {through_ordinal}, expected frontier {}",
                self.latest_ordinal
            );
        }
        Ok(RelayEventPage {
            events,
            through_ordinal,
            through_digest,
        })
    }

    fn desynchronized_detail(
        &self,
        requested_after: u64,
        requested_digest: &str,
    ) -> RelayErrorDetail {
        RelayErrorDetail::Desynchronized {
            requested_after,
            requested_digest: requested_digest.to_owned(),
            earliest_available: self.retained_through,
            earliest_digest: self.retained_digest.clone(),
            latest: self.latest_ordinal,
            latest_digest: self.latest_digest.clone(),
        }
    }
}

/// An attach whose disk work has been lifted out of the relay lock.
///
/// [`DurableRelay::take_deferred_attach`] builds one while holding the lock;
/// [`Self::finish`] then does the reading, decompressing and page assembly
/// with the lock released, so live event recording keeps running underneath a
/// controller's catch-up.
pub struct DeferredRelayAttach {
    request_id: String,
    protocol_version: u32,
    plan: RelayReplayPlan,
    state: RelayOperationalState,
    after_ordinal: u64,
    after_digest: String,
}

impl DeferredRelayAttach {
    /// The journal generation this attach was planned against. Compare it with
    /// [`DurableRelay::journal_generation`] after [`Self::finish`] fails: an
    /// unchanged generation means the failure is real, and a changed one means
    /// the journal was resealed or collected mid-read and the controller
    /// should simply attach again.
    pub fn journal_generation(&self) -> u64 {
        self.plan.generation
    }

    /// Blocking: reads journal segments and decompresses sealed ones. Callers
    /// on an async runtime must run this off the event loop.
    pub fn finish(self) -> RelayResponseEnvelope {
        let body = match self
            .plan
            .attach(self.after_ordinal, &self.after_digest, self.state)
        {
            Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
            Ok(Err(error)) => RelayResponseBody::Error { error },
            Err(error) => RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: format!("{error:#}"),
                    retryable: true,
                    detail: None,
                },
            },
        };
        RelayResponseEnvelope {
            request_id: self.request_id,
            protocol_version: self.protocol_version,
            body,
        }
    }

    /// The answer for an attach whose plan went stale while it was reading:
    /// nothing is wrong with the controller's cursor, so it retries against
    /// the journal as it now stands.
    pub fn stale_journal_response(
        request_id: String,
        protocol_version: u32,
    ) -> RelayResponseEnvelope {
        RelayResponseEnvelope {
            request_id,
            protocol_version,
            body: RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    message: "relay journal was resealed or collected while the replay page was \
                              being read; attach again"
                        .to_owned(),
                    retryable: true,
                    detail: None,
                },
            },
        }
    }
}

fn validate_identifier(value: &str, name: &str) -> Result<()> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid {name}");
    }
    Ok(())
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
