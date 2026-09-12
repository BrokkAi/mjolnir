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
use anyhow::{Context, Result, anyhow, bail, ensure};

use journal::{
    JournalReadMode, RelayJournalSpan, open_relay_journal, read_restored_relay_seed,
    visit_relay_journal_file,
};
use mj_core::archive::CanonicalQueuedCommandKind;
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
        &self,
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
            return Ok(BackgroundTaskStopTarget::ClaudeAsyncTask {
                task_id: task_id.clone(),
            });
        }
        bail!("background task is no longer running")
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
        // The harness that owned those processes is gone, and so is whatever
        // it left running: a restart cannot poll a process it no longer has.
        self.codex_execute_tools.clear();
        self.background_exec_cards.clear();
        self.claude_background_tasks.clear();
        self.kimi_background_tasks.clear();
        self.kimi_provisional_tasks.clear();
        self.kimi_observed_task_ids.clear();
        self.kimi_observed_tool_ids.clear();
        self.agent_terminal_tool_calls.clear();
        if self.background_work == BackgroundWorkPolicy::KimiTasks {
            self.background_work_known = Some(false);
        }
        self.claude_stoppable_tasks.clear();
        self.foreground_tools.clear();
        self.persist_activity_transition()
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

    /// Prove that this native identity was created here and never used.
    /// Codex may not persist a new thread until its first prompt. Missing or
    /// checkpointed history is not evidence that it is safe to replace one.
    pub fn native_session_is_pristine(&self) -> Result<bool> {
        if self.snapshot.recovery_floor_ordinal != 0 {
            return Ok(false);
        }
        let Some(native_id) = self.snapshot.native_session_id.as_deref() else {
            return Ok(false);
        };
        let plan = self.replay_plan();
        let mut ordinal = 0;
        let mut digest = RELAY_EVENT_GENESIS_DIGEST.to_owned();
        let mut locally_created = false;
        while ordinal < self.snapshot.latest_ordinal {
            let page = plan.read_events_after(ordinal, &digest, 64 * 1024)?;
            ensure!(
                !page.events.is_empty(),
                "native session history is incomplete"
            );
            for event in page.events {
                match event.observation {
                    RelayObservation::SessionOpened {
                        native_session_id,
                        resumed,
                    } => {
                        if resumed {
                            return Ok(false);
                        }
                        locally_created = native_session_id == native_id;
                    }
                    RelayObservation::CommandStarted { command_id, .. } => {
                        let Some(dispatch) = self.snapshot.dispatches.get(&command_id) else {
                            return Ok(false);
                        };
                        // Admission records CommandStarted before ACP is
                        // ready. Only a durable claim can have sent it.
                        if matches!(dispatch.command, RelayCommand::Prompt { .. })
                            && !matches!(
                                dispatch.state,
                                RelayDispatchState::Queued | RelayDispatchState::Pending
                            )
                        {
                            return Ok(false);
                        }
                    }
                    RelayObservation::HarnessTurnStarted { .. } => return Ok(false),
                    RelayObservation::SessionUpdate { update }
                        if crate::acp::session_update_has_native_history(&update) =>
                    {
                        return Ok(false);
                    }
                    _ => {}
                }
                ordinal = event.ordinal;
                digest = event.digest;
            }
        }
        Ok(locally_created)
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
            let Some(negotiated) = RelayVersionRange::CURRENT.negotiate(*supported) else {
                return Some(relay_error(
                    RelayErrorCode::IncompatibleProtocol,
                    format!(
                        "controller supports {}-{}, relay supports protocol {}-{}",
                        supported.min,
                        supported.max,
                        RELAY_MIN_PROTOCOL_VERSION,
                        RELAY_PROTOCOL_VERSION
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
        if !envelope.request.supported_at(envelope.protocol_version) {
            return Some(incompatible_request_protocol(envelope.protocol_version));
        }
        None
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
            | RelayRequest::InstallProjectMemorySnapshot { .. } => {
                return Ok(relay_error(
                    RelayErrorCode::InvalidState,
                    "connection-only requests must be handled by the live relay transport",
                    false,
                    None,
                ));
            }
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
        if matches!(command, RelayCommand::CancelTurn) && self.snapshot.checkpoint_barrier.is_some()
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
                        RelayCommand::Cancel | RelayCommand::CancelTurn
                    )
                {
                    return None;
                }
                let running_turn =
                    self.snapshot.active_prompt.is_some() || self.snapshot.harness_turn.is_some();
                if accepted < before_ordinal
                    || (running_turn && matches!(dispatch.command, RelayCommand::CancelTurn))
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
            self.codex_execute_tools.clear();
            self.background_exec_cards.clear();
            self.claude_background_tasks.clear();
            self.kimi_background_tasks.clear();
            self.kimi_provisional_tasks.clear();
            self.kimi_observed_task_ids.clear();
            self.kimi_observed_tool_ids.clear();
            self.agent_terminal_tool_calls.clear();
            if self.background_work == BackgroundWorkPolicy::KimiTasks {
                self.background_work_known = Some(false);
            }
            self.claude_stoppable_tasks.clear();
            self.foreground_tools.clear();
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
        let claude = self.harness_turns == HarnessTurnPolicy::ClaudeAdapter;
        if claude && self.opens_harness_turn(&update) {
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
        if let Some(origin) = settles
            && self.snapshot.harness_turn.is_some()
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
        if finishes_turn {
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
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use test_support::*;

    #[test]
    fn checkpointed_history_cannot_prove_a_native_session_is_pristine() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionOpened {
                native_session_id: "unused".into(),
                resumed: false,
            })
            .unwrap();
        assert!(relay.native_session_is_pristine().unwrap());
        let cursor = ready_checkpoint(&mut relay, "checkpoint");
        submit_floor(&mut relay, "archive-installed", cursor);
        drop(relay);
        let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert!(!relay.native_session_is_pristine().unwrap());
    }

    #[test]
    fn hidden_prompt_context_is_removed_from_harness_visible_text() {
        let text = concat!(
            "<mj-project-memory>private memory</mj-project-memory>\n\n",
            "<user_shell_command>private output</user_shell_command>\n",
            "ship the visible change"
        );

        assert_eq!(strip_hidden_prompt_context(text), "ship the visible change");
        assert_eq!(
            strip_hidden_prompt_context("<mj-project-memory>truncated"),
            ""
        );
        assert_eq!(
            strip_hidden_prompt_context("<user-request>keep me</user-request>"),
            "<user-request>keep me</user-request>"
        );
    }

    #[test]
    fn acp_activity_clock_is_shared_with_operational_status_but_not_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(relay.operational_state().last_acp_activity_at_ms, None);
        relay.acp_activity_clock().mark();
        assert!(relay.operational_state().last_acp_activity_at_ms.is_some());

        let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(reopened.operational_state().last_acp_activity_at_ms, None);
    }

    #[test]
    fn restored_native_identity_waits_for_current_acp_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionOpened {
                native_session_id: "native-session".into(),
                resumed: false,
            })
            .unwrap();
        assert!(!relay.operational_state().native_session_is_ready());
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        assert!(relay.operational_state().native_session_is_ready());
        drop(relay);

        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(
            relay.operational_state().native_session_id.as_deref(),
            Some("native-session")
        );
        assert_eq!(relay.operational_state().acp_ready, Some(false));
        assert!(!relay.operational_state().native_session_is_ready());
        for transition in [
            RelayObservation::SessionRestarted,
            RelayObservation::Closing,
            RelayObservation::Closed,
        ] {
            relay
                .record_observation(RelayObservation::SessionConfigured {
                    config_options: Vec::new(),
                })
                .unwrap();
            assert_eq!(relay.operational_state().acp_ready, Some(true));
            relay.record_observation(transition).unwrap();
            assert_eq!(relay.operational_state().acp_ready, Some(false));
        }
    }

    #[test]
    fn legacy_snapshot_keeps_its_live_turn_when_activity_clocks_are_upgraded() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        relay
            .record_observation(RelayObservation::HarnessTurnStarted {
                started_at_ms: 12_000,
            })
            .unwrap();
        let mut legacy = serde_json::to_value(&relay.snapshot).unwrap();
        legacy["format_version"] = serde_json::json!(4);
        legacy
            .as_object_mut()
            .unwrap()
            .remove("activity_turn_started_at_ms");
        drop(relay);
        fs::write(
            temp.path().join(RELAY_STATE_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        assert_eq!(relay.snapshot.format_version, RELAY_STATE_VERSION);
        assert_eq!(
            relay.operational_state().activity_turn_started_at_ms,
            Some(12_000)
        );
        assert_eq!(
            relay
                .operational_state()
                .harness_turn
                .unwrap()
                .started_at_ms,
            12_000
        );
        relay.persist_snapshot().unwrap();
        drop(relay);
        let reopened = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
        assert_eq!(
            reopened.operational_state().activity_turn_started_at_ms,
            Some(12_000)
        );
    }

    #[test]
    fn idle_clock_starts_at_settlement_survives_reopen_and_ignores_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        assert_eq!(relay.operational_state().idle_since_ms, None);
        relay.record_session_update(tool_call_update()).unwrap();
        assert_eq!(relay.operational_state().idle_since_ms, None);
        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();
        let idle_since = relay.operational_state().idle_since_ms;
        assert!(idle_since.is_some());
        relay
            .record_observation(RelayObservation::Warning {
                message: "metadata does not change activity".into(),
            })
            .unwrap();
        assert_eq!(relay.operational_state().idle_since_ms, idle_since);
        drop(relay);
        let mut relay = claude_relay(temp.path());
        assert_eq!(relay.operational_state().idle_since_ms, idle_since);
        relay.record_session_update(tool_call_update()).unwrap();
        assert_eq!(relay.operational_state().idle_since_ms, None);
    }

    #[test]
    fn relay_store_identity_survives_restart_but_distinguishes_a_fresh_destination() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let relay = DurableRelay::open(&first, "session-one", "test").unwrap();
        let identity = relay.operational_state().store_id.unwrap();
        drop(relay);
        let reopened = DurableRelay::open(&first, "session-one", "test").unwrap();
        assert_eq!(
            reopened.operational_state().store_id.as_deref(),
            Some(identity.as_str())
        );
        let fresh =
            DurableRelay::open(directory.path().join("fresh"), "session-one", "test").unwrap();
        assert_ne!(
            fresh.operational_state().store_id.as_deref(),
            Some(identity.as_str())
        );
    }

    #[test]
    fn idle_clock_waits_for_background_work_and_persists_its_completion() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();
        let turn_start = relay.operational_state().activity_turn_started_at_ms;
        assert!(turn_start.is_some());
        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "background".into(),
                command: "build".into(),
                started_at_ms: 1_000,
            })
            .unwrap();
        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();
        assert_eq!(relay.operational_state().idle_since_ms, None);
        assert_eq!(
            relay.operational_state().activity_turn_started_at_ms,
            turn_start
        );
        // A newly attached client receives the original clock from the worker.
        let reattached: RelayOperationalState =
            serde_json::from_slice(&serde_json::to_vec(&relay.operational_state()).unwrap())
                .unwrap();
        assert_eq!(reattached.activity_turn_started_at_ms, turn_start);
        let stored: RelaySnapshot =
            serde_json::from_slice(&fs::read(temp.path().join(RELAY_STATE_FILE)).unwrap()).unwrap();
        assert_eq!(stored.activity_turn_started_at_ms, turn_start);
        let next_turn = turn_start.unwrap() + 1_000;
        relay
            .record_observation(RelayObservation::HarnessTurnStarted {
                started_at_ms: next_turn,
            })
            .unwrap();
        assert_eq!(
            relay.operational_state().activity_turn_started_at_ms,
            Some(next_turn)
        );
        relay
            .record_observation(RelayObservation::HarnessTurnSettled {
                origin: None,
                prompt_in_flight: false,
            })
            .unwrap();
        assert_eq!(
            relay.operational_state().activity_turn_started_at_ms,
            Some(next_turn)
        );
        relay.agent_terminal_closed("background").unwrap();
        assert_eq!(relay.operational_state().activity_turn_started_at_ms, None);
        let idle_since = relay.operational_state().idle_since_ms;
        assert!(idle_since.is_some());
        drop(relay);
        assert_eq!(
            claude_relay(temp.path()).operational_state().idle_since_ms,
            idle_since
        );
    }

    #[test]
    fn legacy_idle_snapshot_does_not_invent_an_idle_start() {
        let temp = tempfile::tempdir().unwrap();
        let relay = claude_relay(temp.path());
        let mut value = serde_json::to_value(&relay.snapshot).unwrap();
        value.as_object_mut().unwrap().remove("idle_since_ms");
        value.as_object_mut().unwrap().remove("activity_was_idle");
        drop(relay);
        fs::write(
            temp.path().join(RELAY_STATE_FILE),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .record_observation(RelayObservation::Warning {
                message: "old worker".into(),
            })
            .unwrap();
        assert_eq!(relay.operational_state().idle_since_ms, None);
        let mut operational = serde_json::to_value(relay.operational_state()).unwrap();
        operational.as_object_mut().unwrap().remove("idle_since_ms");
        assert_eq!(
            serde_json::from_value::<RelayOperationalState>(operational)
                .unwrap()
                .idle_since_ms,
            None
        );
    }

    #[test]
    fn step_clock_is_shared_with_operational_status_but_not_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(relay.operational_state().current_step_started_at_ms, None);
        relay.step_clock().begin_turn();
        assert!(
            relay
                .operational_state()
                .current_step_started_at_ms
                .is_some()
        );
        relay.step_clock().end_turn();
        assert_eq!(
            relay.operational_state().current_step_started_at_ms,
            None,
            "a finished turn leaves no step in flight"
        );

        let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(
            reopened.operational_state().current_step_started_at_ms,
            None
        );
    }

    #[test]
    fn hidden_context_waits_for_a_prompt_and_survives_an_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let installed = relay.handle(relay_request(
            "install-context",
            RelayRequest::InstallPromptContext {
                text: "<hel-background>memory</hel-background>".into(),
            },
        ));
        assert!(matches!(
            installed.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::PromptContextInstalled
            }
        ));

        submit_relay(
            &mut relay,
            "configure-first",
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "default".into(),
            },
        );
        let config = relay.claim_pending_commands(true).unwrap();
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].hidden_prompt_context, None);
        relay
            .record_command_completed("configure-first", RelayCommandOutcome::Configured)
            .unwrap();

        submit_relay(&mut relay, "first-prompt", prompt("do it"));
        let first = relay.claim_pending_commands(true).unwrap();
        assert_eq!(
            first[0].hidden_prompt_context.as_deref(),
            Some("<hel-background>memory</hel-background>")
        );
        assert_eq!(first[0].command, prompt("do it"));
        relay
            .record_command_interrupted("first-prompt", "restart")
            .unwrap();

        submit_relay(&mut relay, "second-prompt", prompt("continue"));
        let second = relay.claim_pending_commands(true).unwrap();
        assert_eq!(
            second[0].hidden_prompt_context.as_deref(),
            Some("<hel-background>memory</hel-background>")
        );
        relay
            .record_command_completed(
                "second-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();

        submit_relay(&mut relay, "third-prompt", prompt("again"));
        let third = relay.claim_pending_commands(true).unwrap();
        assert_eq!(third[0].hidden_prompt_context, None);
    }

    /// A history large enough to seal several segments and to overflow one
    /// replay page, so an attach against it really does read and decompress.
    fn record_paged_history(relay: &mut DurableRelay, events: usize) {
        for index in 0..events {
            relay
                .record_observation(RelayObservation::Warning {
                    message: format!("{index:04}:{}", "x".repeat(64 * 1024)),
                })
                .unwrap();
        }
    }

    fn attach_envelope(
        relay: &DurableRelay,
        request_id: &str,
        after_ordinal: u64,
    ) -> RelayRequestEnvelope {
        relay_request(
            request_id,
            RelayRequest::Attach {
                after_ordinal,
                after_digest: relay.digest_at(after_ordinal).unwrap().unwrap(),
            },
        )
    }

    #[test]
    fn a_deferred_attach_reads_its_page_while_the_relay_keeps_recording() {
        let temp = tempfile::tempdir().unwrap();
        let relay = Arc::new(Mutex::new(
            DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap(),
        ));
        record_paged_history(&mut relay.lock().unwrap(), 80);
        let planned_frontier = relay.lock().unwrap().latest_ordinal();

        let deferred = {
            let guard = relay.lock().unwrap();
            guard
                .take_deferred_attach(&attach_envelope(&guard, "catch-up", 0))
                .expect("an attach is deferred off the relay lock")
        };

        // The catch-up page has not been assembled yet, and the relay is free.
        for index in 0..8 {
            relay
                .lock()
                .expect("the relay lock is not held by the pending replay")
                .record_observation(RelayObservation::Warning {
                    message: format!("live-{index}"),
                })
                .unwrap();
        }
        let live_frontier = relay.lock().unwrap().latest_ordinal();
        assert_eq!(live_frontier, planned_frontier + 8);

        let response = deferred.finish();
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    events,
                    through_ordinal,
                    through_digest,
                    state,
                },
        } = response.body
        else {
            panic!("deferred attach failed");
        };
        assert!(
            !events.is_empty() && through_ordinal < planned_frontier,
            "the history should not fit in one page: {through_ordinal} of {planned_frontier}"
        );
        // The page is one unbroken run of the chain the cursor asked for, and
        // it reports the frontier captured with the plan rather than the one
        // the live appends moved it to.
        let mut cursor = RelayCursor {
            ordinal: 0,
            digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        };
        for event in &events {
            validate_relay_event(cursor.ordinal, &cursor.digest, event).unwrap();
            cursor.ordinal = event.ordinal;
            cursor.digest.clone_from(&event.digest);
        }
        assert_eq!(cursor.ordinal, through_ordinal);
        assert_eq!(cursor.digest, through_digest);
        assert_eq!(state.latest_ordinal, planned_frontier);
    }

    #[test]
    fn a_proven_replay_cursor_does_not_reread_its_old_segment() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        record_paged_history(&mut relay, 80);
        let first_request = attach_envelope(&relay, "first", 0);
        let first = relay.handle(first_request);
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    through_ordinal,
                    through_digest,
                    ..
                },
        } = &first.body
        else {
            panic!("first replay page failed: {:?}", first.body);
        };
        assert!(*through_ordinal < relay.latest_ordinal());
        relay.remember_replay_cursor(&first);

        let old_segment = relay
            .journal_spans
            .iter()
            .find(|span| {
                *through_ordinal > span.after_ordinal && *through_ordinal <= span.file_last_ordinal
            })
            .expect("the replay cursor belongs to a journal segment")
            .path
            .clone();
        std::fs::rename(&old_segment, old_segment.with_extension("moved")).unwrap();

        assert_eq!(
            relay.digest_at(*through_ordinal).unwrap().as_deref(),
            Some(through_digest.as_str()),
            "validating the returned cursor must not reopen its old segment"
        );
    }

    #[test]
    fn a_deferred_attach_refuses_a_page_from_a_collected_journal() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        record_paged_history(&mut relay, 80);
        let sealed = |root: &Path| {
            fs::read_dir(root.join(RELAY_JOURNAL_DIR))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|name| name == "gz"))
                .count()
        };
        assert!(sealed(temp.path()) >= 2, "history did not seal segments");

        let deferred = relay
            .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
            .expect("an attach is deferred off the relay lock");
        let generation = deferred.journal_generation();

        // Another controller acknowledges the whole history, which rewrites
        // the journal and deletes every sealed segment this plan named.
        let frontier = relay.latest_ordinal();
        let frontier_digest = relay.latest_digest().to_owned();
        submit_floor(
            &mut relay,
            "floor-command-0001",
            RelayCursor {
                ordinal: frontier,
                digest: frontier_digest,
            },
        );
        acknowledge_relay(&mut relay, "ack-everything", frontier);
        assert_eq!(sealed(temp.path()), 0, "collection kept sealed segments");

        let response = deferred.finish();
        let RelayResponseBody::Error { error } = &response.body else {
            panic!("a page read from a collected journal must not be served: {response:?}");
        };
        assert!(
            error.retryable,
            "a collected journal is retryable, not a controller fault: {error:?}"
        );
        assert_ne!(
            relay.journal_generation(),
            generation,
            "collection must mark captured replay plans stale"
        );
    }

    /// A replay page is assembled from files the relay lock no longer guards,
    /// so a span whose file was pruned under the reader must fail the read.
    /// Silently contributing nothing would answer a catch-up with a page that
    /// claims to reach the frontier while carrying none of the events.
    #[test]
    fn a_deferred_attach_never_serves_a_torn_page_after_its_segments_are_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        record_paged_history(&mut relay, 80);
        let frontier = relay.latest_ordinal();

        let deferred = relay
            .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
            .expect("an attach is deferred off the relay lock");
        for entry in fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR)).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }

        let response = deferred.finish();
        match response.body {
            RelayResponseBody::Error { error } => assert!(
                error.retryable,
                "a pruned segment is retryable, not a controller fault: {error:?}"
            ),
            RelayResponseBody::Ok {
                payload:
                    RelayResponsePayload::Attached {
                        events,
                        through_ordinal,
                        ..
                    },
            } => panic!(
                "served {} events but claimed to reach event {through_ordinal} of {frontier}",
                events.len()
            ),
            other => panic!("unexpected attach response: {other:?}"),
        }
    }

    #[test]
    fn a_deferred_attach_refuses_a_page_from_a_resealed_segment() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        // Fill the active segment without crossing its seal threshold.
        record_paged_history(&mut relay, 8);
        let deferred = relay
            .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
            .expect("an attach is deferred off the relay lock");
        let generation = deferred.journal_generation();

        // The next append seals the active segment, moving every event the
        // plan expects to find in `active.jsonl` into a compressed file.
        record_paged_history(&mut relay, 12);
        assert_ne!(relay.journal_generation(), generation);

        let response = deferred.finish();
        let RelayResponseBody::Error { error } = &response.body else {
            panic!("a page read from a resealed segment must not be served: {response:?}");
        };
        assert!(
            error.retryable,
            "a resealed segment is retryable: {error:?}"
        );
    }

    #[test]
    fn relay_runs_queued_prompts_in_order_without_a_controller() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "command-one", prompt("one"));
        submit_relay(&mut relay, "command-two", prompt("two"));
        submit_relay(&mut relay, "command-three", prompt("three"));

        let first = relay.claim_pending_commands(true).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].command_id, "command-one");
        relay
            .record_command_completed(
                "command-one",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        let second = relay.claim_pending_commands(true).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].command_id, "command-two");

        // A relay/process restart interrupts only the command actually handed
        // to ACP and then continues the durable offline queue.
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let third = relay.claim_pending_commands(true).unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].command_id, "command-three");
        assert!(retained_events(&relay).iter().any(|event| matches!(
            &event.observation,
            RelayObservation::CommandInterrupted { command_id, .. }
                if command_id == "command-two"
        )));
    }

    #[test]
    fn duplicate_image_command_is_idempotent_after_attachment_loss() {
        let temp = tempfile::tempdir().unwrap();
        let store = mj_core::attachment::AttachmentStore::worker(temp.path());
        let bytes = b"\x89PNG\r\n\x1a\nverified-image".to_vec();
        let reference =
            mj_core::attachment::AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        store.install(&reference, &bytes).unwrap();
        let command = RelayCommand::Prompt {
            prompt: vec![reference.content_block()],
        };
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let ordinal = submit_relay(&mut relay, "image-command", command.clone());
        fs::remove_file(store.root().join(&reference.sha256)).unwrap();
        let latest_before_retry = relay.latest_ordinal();

        let response = relay.handle(relay_request(
            "image-command-retry",
            RelayRequest::Submit {
                command_id: "image-command".into(),
                command,
            },
        ));
        assert!(matches!(
            response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Accepted {
                    command_id,
                    ordinal: accepted_ordinal,
                }
            } if command_id == "image-command" && accepted_ordinal == ordinal
        ));
        assert_eq!(relay.latest_ordinal(), latest_before_retry);
    }

    fn successful_shell(command: &str, stdout: &str) -> UserShellResult {
        UserShellResult {
            command: command.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: Some(0),
            signal: None,
            duration_ms: 12,
            status: UserShellStatus::Exited,
            error: None,
        }
    }

    #[test]
    fn shell_runs_during_an_active_turn_and_barriers_the_later_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "prompt-command-1", prompt("first"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "prompt-command-1"
        );
        submit_relay(
            &mut relay,
            "shell-command-01",
            RelayCommand::RunUserShell {
                command: "printf ready".into(),
            },
        );
        submit_relay(&mut relay, "prompt-command-2", prompt("after shell"));

        let shell = relay.claim_pending_user_shell_commands_up_to(4).unwrap();
        assert_eq!(shell[0].command_id, "shell-command-01");
        relay
            .record_command_completed(
                "prompt-command-1",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        relay
            .record_command_completed(
                "shell-command-01",
                RelayCommandOutcome::UserShell {
                    result: successful_shell("printf ready", "ready"),
                },
            )
            .unwrap();
        let prompt = relay.claim_pending_commands(true).unwrap();
        assert_eq!(prompt[0].command_id, "prompt-command-2");
        assert!(
            prompt[0]
                .hidden_prompt_context
                .as_deref()
                .is_some_and(
                    |context| context.contains("<user_shell_command>") && context.contains("ready")
                )
        );
    }

    #[test]
    fn a_prompt_accepted_before_a_shell_keeps_priority_and_does_not_consume_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "prompt-command-1", prompt("first"));
        relay.claim_pending_commands(true).unwrap();
        submit_relay(&mut relay, "prompt-command-2", prompt("already queued"));
        submit_relay(
            &mut relay,
            "shell-command-01",
            RelayCommand::RunUserShell {
                command: "printf later".into(),
            },
        );
        relay.claim_pending_user_shell_commands_up_to(4).unwrap();
        relay
            .record_command_completed(
                "prompt-command-1",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();

        let prompt = relay.claim_pending_commands(true).unwrap();
        assert_eq!(prompt[0].command_id, "prompt-command-2");
        assert!(prompt[0].hidden_prompt_context.is_none());
    }

    #[test]
    fn a_shell_cancelled_before_launch_still_reaches_the_next_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "shell-command-01",
            RelayCommand::RunUserShell {
                command: "sleep 60".into(),
            },
        );
        submit_relay(
            &mut relay,
            "cancel-shell-01",
            RelayCommand::CancelUserShell {
                shell_command_id: "shell-command-01".into(),
            },
        );

        let claimed = relay.claim_pending_user_shell_commands_up_to(4).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "cancel-shell-01");
        relay
            .record_command_interrupted(
                "shell-command-01",
                "shell command was cancelled before it started",
            )
            .unwrap();
        relay
            .record_command_completed("cancel-shell-01", RelayCommandOutcome::UserShellCancelled)
            .unwrap();

        submit_relay(&mut relay, "prompt-command-1", prompt("what happened?"));
        let prompt = relay.claim_pending_commands(true).unwrap();
        assert!(
            prompt[0]
                .hidden_prompt_context
                .as_deref()
                .is_some_and(|context| {
                    context.contains("status: interrupted")
                        && context.contains("cancelled before it started")
                })
        );
    }

    #[test]
    fn claims_wait_for_the_current_acp_session_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "wait-for-config", prompt("later"));

        assert!(relay.claim_pending_commands(false).unwrap().is_empty());
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "wait-for-config");
    }

    #[test]
    fn checkpoint_barrier_pauses_offline_prompt_promotion_until_release() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "barrier-command",
            RelayCommand::BeginCheckpoint {
                reason: Some("test".into()),
            },
        );
        submit_relay(&mut relay, "after-barrier", prompt("later"));
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "barrier-command");
        relay.record_checkpoint_ready("barrier-command").unwrap();
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        assert!(relay.operational_state().active_prompt.is_none());

        submit_relay(
            &mut relay,
            "release-command",
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: "barrier-command".into(),
            },
        );
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "after-barrier");
    }

    #[test]
    fn controller_disconnect_cannot_leave_checkpoint_barrier_paused() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "barrier-disconnect",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(&mut relay, "queued-offline", prompt("continue"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay.record_checkpoint_ready("barrier-disconnect").unwrap();

        let cancelled = relay
            .cancel_checkpoint_barrier_on_disconnect("barrier-disconnect")
            .unwrap();
        assert!(cancelled.is_some());
        assert!(relay.operational_state().checkpoint_barrier.is_none());
        let next = relay.claim_pending_commands(true).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].command_id, "queued-offline");
    }

    /// The controller releases dispatch once the archive exists on the target,
    /// long before that archive is installed. Journal history must stay put
    /// until an installed archive covers it.
    #[test]
    fn releasing_a_checkpoint_resumes_dispatch_without_moving_the_recovery_floor() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let ready = ready_checkpoint(&mut relay, "released-barrier");
        submit_relay(&mut relay, "queued-during-release", prompt("later"));
        attach_relay(&mut relay, "attach-release", 0);
        acknowledge_relay(&mut relay, "ack-release", ready.ordinal);
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        let floor_before = relay.snapshot.recovery_floor_ordinal;
        let retained_before = relay.snapshot.retained_through();

        submit_relay(
            &mut relay,
            "release-command",
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: "released-barrier".into(),
            },
        );

        assert!(relay.operational_state().checkpoint_barrier.is_none());
        assert!(relay.operational_state().checkpoint_ready.is_none());
        assert_eq!(relay.snapshot.recovery_floor_ordinal, floor_before);
        assert_eq!(relay.snapshot.retained_through(), retained_before);
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "queued-during-release");
        // The released barrier is terminal, so the controller connection that
        // opened it can drop without cancelling anything.
        assert!(
            relay
                .cancel_checkpoint_barrier_on_disconnect("released-barrier")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn releasing_a_checkpoint_requires_that_exact_ready_barrier() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let missing = submit_release(&mut relay, "release-without-barrier", "no-such-barrier");
        assert!(matches!(
            missing.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));

        submit_relay(
            &mut relay,
            "unready-barrier",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        let unready = submit_release(&mut relay, "release-unready", "unready-barrier");
        assert!(matches!(
            unready.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));

        relay.record_checkpoint_ready("unready-barrier").unwrap();
        let wrong = submit_release(&mut relay, "release-wrong", "another-barrier");
        assert!(matches!(
            wrong.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        assert_eq!(
            relay.operational_state().checkpoint_barrier.as_deref(),
            Some("unready-barrier")
        );
    }

    /// Installing the archive is what earns the journal release, and the
    /// recovery floor is how the relay records it.
    #[test]
    fn advancing_the_recovery_floor_releases_history_only_forward_and_on_chain() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let ready = ready_checkpoint(&mut relay, "installed-barrier");
        submit_relay(
            &mut relay,
            "release-installed",
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: "installed-barrier".into(),
            },
        );
        // Acknowledge past the ready cursor so the recovery floor alone decides
        // what the relay retains.
        attach_relay(&mut relay, "attach-floor", 0);
        let acknowledged = relay.latest_ordinal();
        acknowledge_relay(&mut relay, "ack-floor", acknowledged);
        assert_eq!(relay.snapshot.retained_through(), 0);

        let mismatched = submit_floor(
            &mut relay,
            "floor-wrong-digest",
            RelayCursor {
                ordinal: ready.ordinal,
                digest: "b".repeat(64),
            },
        );
        assert!(matches!(
            mismatched.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        let beyond_frontier = RelayCursor {
            ordinal: relay.latest_ordinal() + 1,
            digest: relay.snapshot.latest_digest.clone(),
        };
        let ahead = submit_floor(&mut relay, "floor-ahead", beyond_frontier);
        assert!(matches!(
            ahead.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        assert_eq!(relay.snapshot.recovery_floor_ordinal, 0);

        submit_relay(
            &mut relay,
            "floor-installed",
            RelayCommand::AdvanceRecoveryFloor {
                through: ready.clone(),
            },
        );
        assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
        assert_eq!(relay.snapshot.recovery_floor_digest, ready.digest);
        assert_eq!(relay.snapshot.retained_through(), ready.ordinal);

        let backwards = submit_floor(
            &mut relay,
            "floor-backwards",
            RelayCursor {
                ordinal: 0,
                digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            },
        );
        assert!(matches!(
            backwards.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
    }

    /// The legacy one-step completion is unchanged: it both resumes dispatch
    /// and advances the recovery floor.
    #[test]
    fn completing_a_checkpoint_still_resumes_dispatch_and_advances_the_floor() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let ready = ready_checkpoint(&mut relay, "completed-barrier");
        submit_relay(&mut relay, "queued-during-completion", prompt("later"));
        attach_relay(&mut relay, "attach-completion", 0);
        acknowledge_relay(&mut relay, "ack-completion", ready.ordinal);

        submit_relay(
            &mut relay,
            "complete-command",
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: "completed-barrier".into(),
            },
        );

        assert!(relay.operational_state().checkpoint_barrier.is_none());
        assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
        assert_eq!(relay.snapshot.retained_through(), ready.ordinal);
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "queued-during-completion");
    }

    #[test]
    fn checkpoint_barriers_are_serialized_and_only_exact_completion_releases_them() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "first-barrier",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(
            &mut relay,
            "second-barrier",
            RelayCommand::BeginCheckpoint { reason: None },
        );

        let first = relay.claim_pending_commands(true).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].command_id, "first-barrier");
        relay.record_checkpoint_ready("first-barrier").unwrap();
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        let wrong = relay.handle(relay_request(
            "wrong-completion",
            RelayRequest::Submit {
                command_id: "wrong-complete-command".into(),
                command: RelayCommand::CompleteCheckpoint {
                    barrier_command_id: "second-barrier".into(),
                },
            },
        ));
        assert!(matches!(
            wrong.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));

        submit_relay(
            &mut relay,
            "complete-first",
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: "first-barrier".into(),
            },
        );
        let second = relay.claim_pending_commands(true).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].command_id, "second-barrier");
    }

    #[test]
    fn checkpoint_waits_for_earlier_queued_control_and_freezes_later_control() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "config-before",
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "before".into(),
            },
        );
        submit_relay(
            &mut relay,
            "control-barrier",
            RelayCommand::BeginCheckpoint { reason: None },
        );

        let control = relay.claim_pending_commands(true).unwrap();
        assert_eq!(control.len(), 1);
        assert_eq!(control[0].command_id, "config-before");
        relay
            .record_command_completed("config-before", RelayCommandOutcome::Configured)
            .unwrap();
        assert_eq!(relay.operational_state().config["model"], "before");

        let barrier = relay.claim_pending_commands(true).unwrap();
        assert_eq!(barrier.len(), 1);
        assert_eq!(barrier[0].command_id, "control-barrier");
        relay.record_checkpoint_ready("control-barrier").unwrap();
        submit_relay(
            &mut relay,
            "config-after",
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "after".into(),
            },
        );
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        submit_relay(
            &mut relay,
            "complete-control-barrier",
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: "control-barrier".into(),
            },
        );
        let later = relay.claim_pending_commands(true).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].command_id, "config-after");
    }

    #[test]
    fn a_recorded_notice_becomes_one_verbatim_system_line() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let text = "This session moved from /home/dev/project into a container.";

        submit_relay(
            &mut relay,
            "resume-notice-1",
            RelayCommand::RecordNotice { text: text.into() },
        );

        // A notice never reaches ACP: it completes inside the relay.
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        let mut session = mj_core::state::MaterializedSession::empty(SESSION);
        for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
            let projected = mj_core::projection::project_relay_event(&session, &event).unwrap();
            mj_core::projection::apply_committed_projection_event(
                &mut session,
                &event,
                projected.mutation,
            )
            .unwrap();
        }

        let notices = session
            .transcript
            .iter()
            .filter_map(|item| match &item.body {
                mj_core::state::TranscriptBody::System { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices, vec![text.to_owned()]);
    }

    #[test]
    fn a_repeated_notice_append_still_leaves_one_conversation_line() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let text = "The working tree moved while this session was stopped.";
        submit_relay(
            &mut relay,
            "resume-notice-1",
            RelayCommand::RecordNotice { text: text.into() },
        );
        // Stand in for a retry that re-appended the notice after a transient
        // persistence failure reported a durable append as unfinished.
        relay
            .append_relay_event(
                Some("resume-notice-1"),
                RelayObservation::Notice {
                    message: text.into(),
                },
            )
            .unwrap();

        let mut session = mj_core::state::MaterializedSession::empty(SESSION);
        for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
            let projected = mj_core::projection::project_relay_event(&session, &event).unwrap();
            mj_core::projection::apply_committed_projection_event(
                &mut session,
                &event,
                projected.mutation,
            )
            .unwrap();
        }

        assert_eq!(session.transcript.len(), 1);
    }

    /// Build a relay that models the turns Claude Code starts on its own.
    fn claude_relay(root: &std::path::Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, SESSION, "1.0.0").unwrap();
        relay.set_harness_turn_policy(HarnessTurnPolicy::ClaudeAdapter);
        relay.set_background_work_policy(BackgroundWorkPolicy::ClaudeTasks);
        relay
    }

    fn tool_call_update() -> SessionUpdate {
        SessionUpdate::ToolCall(agent_client_protocol::schema::v1::ToolCall::new(
            "call-1", "Bash",
        ))
    }

    /// The `usage_update` the Claude adapter sends when an SDK turn ends.
    fn settling_usage_update(origin: &str) -> SessionUpdate {
        let mut usage = agent_client_protocol::schema::v1::UsageUpdate::new(10, 200);
        usage.meta = Some(
            serde_json::from_value(serde_json::json!({
                CLAUDE_ORIGIN_META_KEY: {"kind": origin},
            }))
            .unwrap(),
        );
        SessionUpdate::UsageUpdate(usage)
    }

    fn observations(relay: &DurableRelay) -> Vec<RelayObservation> {
        relay
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .into_iter()
            .map(|event| event.observation)
            .collect()
    }

    #[test]
    fn agent_output_at_idle_opens_a_harness_turn_and_the_origin_marker_settles_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());

        relay.record_session_update(tool_call_update()).unwrap();

        let running = relay.operational_state();
        assert_eq!(running.execution, RelayExecutionState::Running);
        assert!(running.active_prompt.is_none(), "no prompt is in flight");
        let turn = running.harness_turn.expect("a harness turn is open");
        assert!(turn.started_at_ms > 0);
        assert_eq!(running.last_harness_turn_started_ordinal, Some(1));
        assert!(matches!(
            observations(&relay).as_slice(),
            [
                RelayObservation::HarnessTurnStarted { .. },
                RelayObservation::SessionUpdate { .. }
            ]
        ));

        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();

        let settled = relay.operational_state();
        assert_eq!(settled.execution, RelayExecutionState::Idle);
        assert!(settled.harness_turn.is_none());
        assert_eq!(
            settled.foreground_tool_started_at_ms, None,
            "the confirmed turn boundary clears any tool status the adapter left open"
        );
        assert_eq!(
            settled.last_harness_turn_started_ordinal,
            Some(1),
            "the started ordinal only moves forward, so a checkpoint can compare against it"
        );
        assert!(matches!(
            observations(&relay).last(),
            Some(RelayObservation::HarnessTurnSettled { origin, .. })
                if origin.as_deref() == Some("task-notification")
        ));
    }

    #[test]
    fn a_harness_turn_holds_the_checkpoint_barrier_until_it_settles() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();

        submit_relay(
            &mut relay,
            "barrier-command",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        assert!(
            relay.claim_pending_commands(true).unwrap().is_empty(),
            "a turn the harness started on its own must keep the barrier queued"
        );

        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();

        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "barrier-command");
    }

    #[test]
    fn a_prompt_queued_during_a_harness_turn_dispatches_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();

        submit_relay(&mut relay, "typed-mid-turn", prompt("answer this too"));
        let claimed = relay.claim_pending_commands(true).unwrap();

        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "typed-mid-turn");
        assert!(
            observations(&relay).iter().any(|observation| matches!(
                observation,
                RelayObservation::CommandStarted { command_id, .. } if command_id == "typed-mid-turn"
            )),
            "the prompt starts while the harness turn is still open"
        );
        assert!(
            relay.operational_state().harness_turn.is_some(),
            "dispatching a prompt does not end the turn the harness started"
        );

        // Barrier priority over queued prompts is an invariant: a pending
        // barrier still freezes a prompt typed after it.
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();
        submit_relay(
            &mut relay,
            "barrier-command",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(&mut relay, "typed-after-barrier", prompt("wait for me"));

        assert!(
            relay.claim_pending_commands(true).unwrap().is_empty(),
            "a pending barrier still outranks a prompt typed during a harness turn"
        );
    }

    #[test]
    fn a_prompt_result_settles_a_lingering_harness_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();
        submit_relay(&mut relay, "next-prompt", prompt("carry on"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

        relay
            .record_command_completed(
                "next-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();

        let state = relay.operational_state();
        assert!(
            state.harness_turn.is_none(),
            "a prompt result means the SDK reached a turn boundary"
        );
        assert_eq!(state.execution, RelayExecutionState::Idle);
        assert_eq!(
            state.foreground_tool_started_at_ms, None,
            "the prompt result outranks a stale pending tool status"
        );
    }

    #[test]
    fn a_restart_clears_a_harness_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();

        relay
            .record_observation(RelayObservation::SessionRestarted)
            .unwrap();

        let state = relay.operational_state();
        assert!(state.harness_turn.is_none());
        assert_eq!(state.execution, RelayExecutionState::Idle);
    }

    /// A Codex `exec_command` card. The adapter reports the result under
    /// `rawOutput`, with `exit_code` null while the process is still running.
    fn exec_card(
        tool_call_id: &'static str,
        command: &[&str],
        exit_code: Option<i64>,
    ) -> SessionUpdate {
        let mut call = agent_client_protocol::schema::v1::ToolCall::new(tool_call_id, "shell");
        call.kind = agent_client_protocol::schema::v1::ToolKind::Execute;
        call.status = agent_client_protocol::schema::v1::ToolCallStatus::Completed;
        call.raw_input = Some(serde_json::json!({ "command": command }));
        call.raw_output = Some(serde_json::json!({
            "output": "",
            "exit_code": exit_code,
        }));
        SessionUpdate::ToolCall(call)
    }

    fn exec_card_update(tool_call_id: &'static str, exit_code: Option<i64>) -> SessionUpdate {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};

        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                .raw_output(serde_json::json!({
                    "output": "",
                    "exit_code": exit_code,
                })),
        ))
    }

    fn kimi_background_agent_card() -> SessionUpdate {
        use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

        let mut call = ToolCall::new(
            "1:tool_agent",
            "Launching background coder agent: Fix memory use",
        );
        call.status = ToolCallStatus::Completed;
        call.raw_input = Some(serde_json::json!({
            "description": "Fix memory use",
            "prompt": "Fix the issue",
            "run_in_background": true
        }));
        call.raw_output = Some(serde_json::Value::String(
            "task_id: agent-deadbeef\nstatus: running\nagent_id: agent-1".into(),
        ));
        SessionUpdate::ToolCall(call)
    }

    fn kimi_background_agent_card_with_run_in_background_only() -> SessionUpdate {
        use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

        let mut call = ToolCall::new("run-only", "agent");
        call.status = ToolCallStatus::Completed;
        call.raw_input = Some(serde_json::json!({
            "description": "Run-only agent",
            "run_in_background": true
        }));
        SessionUpdate::ToolCall(call)
    }

    #[test]
    fn kimi_task_queries_reconcile_by_native_identity_in_either_event_order() {
        use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

        for native_first in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), "kimi-queries", "test").unwrap();
            relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
            relay
                .record_observation(RelayObservation::SessionConfigured {
                    config_options: Vec::new(),
                })
                .unwrap();
            let query = |call_id: &'static str, title: &'static str| {
                let mut call = ToolCall::new(call_id, title);
                call.status = ToolCallStatus::Completed;
                call.raw_input = Some(serde_json::json!({"task_id": "bash-tlqj0v63"}));
                call.raw_output = Some(serde_json::json!({
                    "output": "retrieval_status: not_ready\ntask_id: bash-tlqj0v63\nstatus: running\nparent_tool_call_id: tool-launcher\n"
                }));
                SessionUpdate::ToolCall(call)
            };
            let native = crate::acp::KimiBackgroundTask {
                task_id: "bash-tlqj0v63".into(),
                description: "validation".into(),
                started_at_ms: 1_000,
                parent_tool_call_id: Some("tool-launcher".into()),
            };
            let tools = BTreeSet::from(["tool-launcher".into()]);
            let tasks = BTreeSet::from(["bash-tlqj0v63".into()]);
            if native_first {
                relay
                    .kimi_background_tasks_changed(
                        vec![native.clone()],
                        tools.clone(),
                        tasks.clone(),
                    )
                    .unwrap();
            }
            for (id, title) in [("0:query-one", "TaskOutput"), ("0:query-two", "WaitFor")] {
                relay.record_session_update(query(id, title)).unwrap();
                assert_eq!(relay.operational_state().background_commands.len(), 1);
            }
            relay
                .kimi_background_tasks_changed(vec![native], tools.clone(), tasks.clone())
                .unwrap();
            assert_eq!(relay.operational_state().background_commands.len(), 1);
            relay
                .kimi_background_tasks_changed(Vec::new(), tools, tasks)
                .unwrap();
            assert!(relay.operational_state().background_commands.is_empty());
            relay
                .record_session_update(query("0:late-query", "TaskOutput"))
                .unwrap();
            assert!(relay.operational_state().background_commands.is_empty());
            assert!(relay.operational_state().is_quiet());
        }
    }

    fn kimi_background_agent_card_with_running_task_id_only() -> SessionUpdate {
        use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

        let mut call = ToolCall::new("task-only", "agent");
        call.status = ToolCallStatus::Completed;
        call.raw_input = Some(serde_json::json!({
            "description": "Task-only agent"
        }));
        call.raw_output = Some(serde_json::Value::String(
            "task_id: agent-task-only\nstatus: running".into(),
        ));
        SessionUpdate::ToolCall(call)
    }

    /// Kimi's `Bash` launcher card with `run_in_background`, optionally
    /// embedding the hosted terminal the detached shell runs in.
    fn kimi_background_shell_card(
        tool_call_id: &'static str,
        terminal_id: Option<&str>,
    ) -> SessionUpdate {
        use agent_client_protocol::schema::v1::{
            Terminal, ToolCall, ToolCallContent, ToolCallStatus,
        };

        let mut call = ToolCall::new(tool_call_id, "Bash");
        call.status = ToolCallStatus::Completed;
        call.raw_input = Some(serde_json::json!({
            "command": "cargo build --release",
            "description": "Build release runner",
            "run_in_background": true
        }));
        if let Some(terminal_id) = terminal_id {
            call.content = vec![ToolCallContent::Terminal(Terminal::new(
                terminal_id.to_owned(),
            ))];
        }
        SessionUpdate::ToolCall(call)
    }

    fn kimi_process_task(parent_tool_call_id: &str) -> crate::acp::KimiBackgroundTask {
        crate::acp::KimiBackgroundTask {
            task_id: "bash-r5ae".into(),
            description: "Build release runner".into(),
            started_at_ms: 2_000,
            parent_tool_call_id: Some(parent_tool_call_id.into()),
        }
    }

    /// Put a prompt of ours in flight so the turn-scoped terminal rule applies.
    fn kimi_relay_with_prompt_in_flight(root: &Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(&mut relay, "parent-command", prompt("build it"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay
    }

    #[test]
    fn kimi_detached_shell_is_listed_once_as_its_hosted_terminal_during_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
        relay
            .record_session_update(kimi_background_shell_card("3:tool_bash", Some("term-60")))
            .unwrap();
        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "term-60".into(),
                command: "cargo build --release".into(),
                started_at_ms: 2_000,
            })
            .unwrap();
        relay
            .kimi_background_tasks_changed(
                vec![kimi_process_task("tool_bash")],
                BTreeSet::from(["tool_bash".into()]),
                BTreeSet::new(),
            )
            .unwrap();

        let commands = relay.operational_state().background_commands;
        assert_eq!(
            commands
                .iter()
                .map(|command| command.id.as_str())
                .collect::<Vec<_>>(),
            ["terminal:term-60"],
            "the detached shell is one job, stoppable through its terminal"
        );

        relay.agent_terminal_closed("term-60").unwrap();
        relay
            .kimi_background_tasks_changed(
                Vec::new(),
                BTreeSet::from(["tool_bash".into()]),
                BTreeSet::new(),
            )
            .unwrap();
        assert!(
            relay.operational_state().background_commands.is_empty(),
            "the shell exited, so nothing is left running"
        );
    }

    #[test]
    fn kimi_detached_shell_without_a_bound_terminal_is_listed_as_a_native_task() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
        relay
            .kimi_background_tasks_changed(
                vec![kimi_process_task("tool_bash")],
                BTreeSet::from(["tool_bash".into()]),
                BTreeSet::new(),
            )
            .unwrap();

        let commands = relay.operational_state().background_commands;
        assert_eq!(
            commands
                .iter()
                .map(|command| command.id.as_str())
                .collect::<Vec<_>>(),
            ["kimi:bash-r5ae"],
            "without ACP evidence the native record is all there is"
        );
    }

    #[test]
    fn a_kimi_hosted_terminal_with_no_detachment_evidence_stays_hidden_during_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
        relay
            .record_session_update(kimi_background_shell_card("3:tool_wait", None))
            .unwrap();
        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "term-61".into(),
                command: "cargo test".into(),
                started_at_ms: 2_000,
            })
            .unwrap();

        assert!(
            !relay
                .operational_state()
                .background_commands
                .iter()
                .any(|command| command.id == "terminal:term-61"),
            "a terminal the turn may still be waiting on is the turn's own work"
        );
    }

    #[test]
    fn kimi_agent_acp_evidence_tracks_each_background_alternative_independently() {
        for update in [
            kimi_background_agent_card_with_run_in_background_only(),
            kimi_background_agent_card_with_running_task_id_only(),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
            relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
            relay.record_session_update(update).unwrap();

            let state = relay.operational_state();
            assert_eq!(state.background_commands.len(), 1);
            assert_eq!(state.background_work_known, Some(false));
            assert!(!state.is_quiet());
        }
    }

    #[test]
    fn unmatched_kimi_provisional_work_survives_empty_native_scan_until_evidence_or_teardown() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .record_session_update(kimi_background_agent_card())
            .unwrap();

        relay
            .kimi_background_tasks_changed(Vec::new(), BTreeSet::new(), BTreeSet::new())
            .unwrap();
        assert_eq!(relay.operational_state().background_commands.len(), 1);

        relay
            .kimi_background_tasks_changed(
                vec![crate::acp::KimiBackgroundTask {
                    task_id: "agent-deadbeef".into(),
                    description: "Fix memory use".into(),
                    started_at_ms: 1_000,
                    parent_tool_call_id: Some("tool_agent".into()),
                }],
                BTreeSet::from(["tool_agent".into()]),
                BTreeSet::new(),
            )
            .unwrap();
        let state = relay.operational_state();
        assert_eq!(state.background_commands.len(), 1);
        assert_eq!(state.background_commands[0].id, "kimi:agent-deadbeef");

        for observation in [
            RelayObservation::SessionRestarted,
            RelayObservation::Closing,
            RelayObservation::Closed,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
            relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
            relay
                .record_session_update(kimi_background_agent_card())
                .unwrap();
            relay
                .kimi_background_tasks_changed(Vec::new(), BTreeSet::new(), BTreeSet::new())
                .unwrap();
            relay.record_observation(observation).unwrap();
            let state = relay.operational_state();
            assert!(state.background_commands.is_empty());
            assert_eq!(state.background_work_known, Some(false));
        }
    }

    #[test]
    fn kimi_background_agent_survives_its_parent_prompt_until_native_termination() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(&mut relay, "parent-command", prompt("delegate this"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

        relay
            .record_session_update(kimi_background_agent_card())
            .unwrap();
        relay
            .record_command_completed(
                "parent-command",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();

        let provisional = relay.operational_state();
        assert_eq!(provisional.background_commands.len(), 1);
        assert_eq!(provisional.background_work_known, Some(false));
        assert!(!provisional.is_quiet());

        relay
            .kimi_background_tasks_changed(
                vec![crate::acp::KimiBackgroundTask {
                    task_id: "agent-deadbeef".into(),
                    description: "Fix memory use".into(),
                    started_at_ms: 1_000,
                    parent_tool_call_id: Some("tool_agent".into()),
                }],
                BTreeSet::from(["tool_agent".into()]),
                BTreeSet::new(),
            )
            .unwrap();
        let running = relay.operational_state();
        assert_eq!(running.background_commands.len(), 1);
        assert_eq!(running.background_work_known, Some(true));
        assert!(!running.is_quiet());

        relay
            .kimi_background_tasks_changed(
                Vec::new(),
                BTreeSet::from(["tool_agent".into()]),
                BTreeSet::new(),
            )
            .unwrap();
        let terminated = relay.operational_state();
        assert!(terminated.background_commands.is_empty());
        assert_eq!(terminated.background_work_known, Some(true));
        assert!(terminated.is_quiet());
    }

    #[test]
    fn kimi_tracker_failure_retains_work_and_blocks_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .kimi_background_tasks_changed(
                vec![crate::acp::KimiBackgroundTask {
                    task_id: "agent-deadbeef".into(),
                    description: "Fix memory use".into(),
                    started_at_ms: 1_000,
                    parent_tool_call_id: None,
                }],
                BTreeSet::new(),
                BTreeSet::new(),
            )
            .unwrap();

        relay.kimi_background_tasks_unavailable().unwrap();

        let state = relay.operational_state();
        assert_eq!(state.background_commands.len(), 1);
        assert_eq!(state.background_work_known, Some(false));
        assert!(!state.is_quiet());
    }

    #[test]
    fn a_terminal_the_agent_left_running_is_background_work_once_the_turn_ends() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();
        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "terminal-1".into(),
                command: "cargo test".into(),
                started_at_ms: 4_000,
            })
            .unwrap();

        assert!(
            relay.operational_state().background_commands.is_empty(),
            "a terminal is the turn's own work while that turn is still open"
        );

        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();

        assert_eq!(
            relay.operational_state().background_commands,
            vec![BackgroundCommand {
                id: "terminal:terminal-1".into(),
                started_at_ms: 4_000,
                command: "cargo test".into(),
                can_stop: true,
            }],
            "the command outlived the turn that started it"
        );

        relay.agent_terminal_closed("terminal-1").unwrap();
        assert!(
            relay.operational_state().background_commands.is_empty(),
            "the process exited, so there is nothing left running"
        );
    }

    fn claude_task(task_id: &str, description: &str) -> crate::acp::ClaudeBackgroundTask {
        crate::acp::ClaudeBackgroundTask {
            task_id: task_id.into(),
            description: description.into(),
        }
    }

    #[test]
    fn claude_background_tasks_survive_prompt_boundaries_until_the_level_is_empty() {
        for outcome in ["completed", "rejected", "interrupted"] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = claude_relay(temp.path());
            relay
                .record_observation(RelayObservation::SessionConfigured {
                    config_options: Vec::new(),
                })
                .unwrap();
            submit_relay(
                &mut relay,
                "review-prompt",
                prompt("start background reviews"),
            );
            assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
            relay.record_session_update(tool_call_update()).unwrap();
            relay
                .claude_background_tasks_changed(vec![
                    claude_task("design", "Design review"),
                    claude_task("refuter", "Refute findings"),
                ])
                .unwrap();
            match outcome {
                "completed" => relay.record_command_completed(
                    "review-prompt",
                    RelayCommandOutcome::Prompt {
                        stop_reason: "end_turn".into(),
                        usage: None,
                    },
                ),
                "rejected" => relay.record_command_rejected("review-prompt", "adapter failed"),
                _ => relay.record_command_interrupted("review-prompt", "cancelled"),
            }
            .unwrap();

            let state = relay.operational_state();
            assert_eq!(state.execution, RelayExecutionState::Idle, "{outcome}");
            assert!(state.harness_turn.is_none());
            assert!(state.foreground_tool_started_at_ms.is_none());
            assert_eq!(state.background_commands.len(), 2);
            assert!(
                !state.is_quiet(),
                "background agents must prevent worker replacement"
            );

            // A replacement level clears missing tasks even without a completion
            // bookend, and keeps the clock of a task that remains live.
            relay
                .claude_background_tasks
                .get_mut("design")
                .unwrap()
                .started_at_ms = 123;
            relay
                .claude_background_tasks_changed(vec![claude_task("design", "Design cleanup")])
                .unwrap();
            assert_eq!(
                relay.operational_state().background_commands,
                vec![BackgroundCommand {
                    id: "claude:design".into(),
                    started_at_ms: 123,
                    command: "Design cleanup".into(),
                    can_stop: false,
                }]
            );
            relay.claude_background_tasks_changed(Vec::new()).unwrap();
            assert!(relay.operational_state().is_quiet());
        }
    }

    #[test]
    fn background_task_stop_targets_are_live_capability_checked_and_namespaced() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "shared-id".into(),
                command: "cargo test".into(),
                started_at_ms: 10,
            })
            .unwrap();
        relay
            .claude_background_tasks_changed(vec![claude_task("shared-id", "Review tests")])
            .unwrap();

        assert_eq!(
            relay
                .background_task_stop_target("terminal:shared-id")
                .unwrap(),
            BackgroundTaskStopTarget::HostedTerminal {
                terminal_id: "shared-id".into(),
            }
        );
        assert!(
            relay
                .background_task_stop_target("claude:shared-id")
                .is_err(),
            "Claude must not be stoppable until AIR grants the affordance"
        );

        relay
            .claude_async_task_control_changed("shared-id".into(), true)
            .unwrap();
        assert_eq!(
            relay
                .background_task_stop_target("claude:shared-id")
                .unwrap(),
            BackgroundTaskStopTarget::ClaudeAsyncTask {
                task_id: "shared-id".into(),
            }
        );
        assert!(
            relay
                .operational_state()
                .background_commands
                .iter()
                .any(|command| command.id == "claude:shared-id" && command.can_stop)
        );

        relay
            .claude_async_task_control_changed("shared-id".into(), false)
            .unwrap();
        assert!(
            relay
                .background_task_stop_target("claude:shared-id")
                .is_err()
        );
        relay.agent_terminal_closed("shared-id").unwrap();
        assert!(
            relay
                .background_task_stop_target("terminal:shared-id")
                .is_err(),
            "a stale UI id must never resolve after its task exits"
        );
    }

    #[test]
    fn claude_background_levels_do_not_open_turns_or_enter_the_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        let ordinal = relay.snapshot.latest_ordinal;
        relay
            .claude_background_tasks_changed(vec![claude_task("workflow", "Design reviews")])
            .unwrap();
        assert_eq!(relay.snapshot.latest_ordinal, ordinal);
        assert_eq!(
            relay.operational_state().execution,
            RelayExecutionState::Idle
        );
        assert!(relay.operational_state().harness_turn.is_none());

        // An autonomous follow-up keeps its foreground state while tasks live,
        // and settling that turn returns to background work.
        relay.record_session_update(tool_call_update()).unwrap();
        assert_eq!(
            relay.operational_state().execution,
            RelayExecutionState::Running
        );
        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();
        assert_eq!(
            relay.operational_state().execution,
            RelayExecutionState::Idle
        );
        assert_eq!(relay.operational_state().background_commands.len(), 1);

        relay
            .agent_terminal_started(ActiveAgentTerminal {
                terminal_id: "shell".into(),
                command: "sleep 600".into(),
                started_at_ms: 1,
            })
            .unwrap();
        assert_eq!(relay.operational_state().background_commands.len(), 2);
        relay.claude_background_tasks_changed(Vec::new()).unwrap();
        assert_eq!(relay.operational_state().background_commands.len(), 1);
        relay.agent_terminal_closed("shell").unwrap();
        assert!(relay.operational_state().is_quiet());
    }

    #[test]
    fn claude_background_tasks_are_process_local_and_clear_on_teardown() {
        for observation in [
            RelayObservation::SessionRestarted,
            RelayObservation::Closing,
            RelayObservation::Closed,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = claude_relay(temp.path());
            relay
                .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
                .unwrap();
            relay.record_observation(observation).unwrap();
            assert!(relay.operational_state().background_commands.is_empty());
        }
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
            .unwrap();
        relay.clear_agent_terminals().unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
        relay
            .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
            .unwrap();
        drop(relay);
        let relay = claude_relay(temp.path());
        assert!(relay.operational_state().background_commands.is_empty());
    }

    #[test]
    fn a_codex_exec_card_without_an_exit_code_is_background_work_until_one_arrives() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);

        let mut started = agent_client_protocol::schema::v1::ToolCall::new("call-1", "shell");
        started.kind = agent_client_protocol::schema::v1::ToolKind::Execute;
        started.status = agent_client_protocol::schema::v1::ToolCallStatus::InProgress;
        started.raw_input = Some(serde_json::json!({
            "command": ["bash", "-lc", "sleep 600"],
        }));
        relay
            .record_session_update(SessionUpdate::ToolCall(started))
            .unwrap();
        assert!(
            relay.operational_state().background_commands.is_empty(),
            "an execute card is not background work before it reports a detached result"
        );

        relay
            .record_session_update(exec_card_update("call-1", None))
            .unwrap();

        let commands = relay.operational_state().background_commands;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].id, "codex:call-1");
        assert_eq!(commands[0].command, "bash -lc sleep 600");
        assert!(commands[0].started_at_ms > 0);
        assert!(!commands[0].can_stop);
        assert!(relay.background_task_stop_target("codex:call-1").is_err());

        // A card that does report an exit code says the process is done, even
        // when it is the first card for that call.
        relay
            .record_session_update(exec_card("call-2", &["ls"], Some(0)))
            .unwrap();
        assert_eq!(
            relay.operational_state().background_commands.len(),
            1,
            "a finished command is not background work"
        );

        // Codex polls the process it left running; the poll carries the exit.
        relay
            .record_session_update(exec_card_update("call-1", Some(0)))
            .unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
    }

    #[test]
    fn completed_codex_mcp_execute_calls_do_not_leave_background_work() {
        use agent_client_protocol::schema::v1::{
            ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
        };

        for partial in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
            relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
            relay
                .record_observation(RelayObservation::SessionConfigured {
                    config_options: Vec::new(),
                })
                .unwrap();
            submit_relay(&mut relay, "memory-prompt", prompt("remember the result"));
            assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

            let mut call = ToolCall::new("memory", "mcp.mj-project-memory.memory_write");
            call.kind = ToolKind::Execute;
            call.raw_input = Some(serde_json::json!({
                "server": "mj-project-memory", "tool": "memory_write",
                "arguments": {"path": "/MEMORY.md", "content": "done"}
            }));
            let output = serde_json::json!({
                "result": {"content": [{"type": "text", "text": "saved"}]},
                "error": null
            });
            if partial {
                call.status = ToolCallStatus::InProgress;
                relay
                    .record_session_update(SessionUpdate::ToolCall(call))
                    .unwrap();
                relay
                    .record_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        "memory",
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .raw_output(output),
                    )))
                    .unwrap();
            } else {
                call.status = ToolCallStatus::Completed;
                call.raw_output = Some(output);
                relay
                    .record_session_update(SessionUpdate::ToolCall(call))
                    .unwrap();
            }
            assert!(relay.operational_state().background_commands.is_empty());
            relay
                .record_command_completed(
                    "memory-prompt",
                    RelayCommandOutcome::Prompt {
                        stop_reason: "end_turn".into(),
                        usage: None,
                    },
                )
                .unwrap();
            assert!(relay.operational_state().is_quiet());
        }
    }

    #[test]
    fn a_codex_non_execute_partial_result_is_not_background_work() {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);

        let mut guardian = agent_client_protocol::schema::v1::ToolCall::new(
            "guardian-assessment",
            "Guardian Review",
        );
        guardian.kind = agent_client_protocol::schema::v1::ToolKind::Think;
        guardian.status = agent_client_protocol::schema::v1::ToolCallStatus::InProgress;
        relay
            .record_session_update(SessionUpdate::ToolCall(guardian))
            .unwrap();
        relay
            .record_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "guardian-assessment",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    .raw_output(serde_json::json!({"review": {"status": "approved"}})),
            )))
            .unwrap();

        assert!(
            relay.operational_state().background_commands.is_empty(),
            "a partial result inherits the original non-execute kind"
        );
    }

    #[test]
    fn prompt_boundaries_clear_stale_tools_but_preserve_detached_commands() {
        for outcome in ["completed", "rejected", "interrupted"] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
            relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
            submit_relay(&mut relay, "boundary-prompt", prompt("run work"));
            assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
            relay.record_session_update(tool_call_update()).unwrap();
            relay
                .record_session_update(exec_card("detached", &["sleep", "600"], None))
                .unwrap();
            assert!(
                relay
                    .operational_state()
                    .foreground_tool_started_at_ms
                    .is_some()
            );

            match outcome {
                "completed" => relay.record_command_completed(
                    "boundary-prompt",
                    RelayCommandOutcome::Prompt {
                        stop_reason: "end_turn".into(),
                        usage: None,
                    },
                ),
                "rejected" => relay.record_command_rejected("boundary-prompt", "adapter failed"),
                _ => relay.record_command_interrupted("boundary-prompt", "cancelled"),
            }
            .unwrap();

            let state = relay.operational_state();
            assert_eq!(state.foreground_tool_started_at_ms, None, "{outcome}");
            assert_eq!(state.background_commands.len(), 1, "{outcome}");
            relay
                .record_session_update(exec_card_update("detached", Some(0)))
                .unwrap();
            assert!(
                relay.operational_state().background_commands.is_empty(),
                "{outcome}"
            );
        }
    }

    #[test]
    fn an_unsettled_tool_call_is_foreground_work_even_without_a_turn_marker() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();

        relay.record_session_update(tool_call_update()).unwrap();
        let state = relay.operational_state();
        assert!(
            state.foreground_tool_started_at_ms.is_some(),
            "a pending tool is positive foreground-work evidence"
        );
        assert!(
            !state.is_quiet(),
            "a pending foreground tool blocks replacement"
        );

        relay
            .record_session_update(exec_card("call-1", &["true"], Some(0)))
            .unwrap();
        let state = relay.operational_state();
        assert_eq!(
            state.foreground_tool_started_at_ms, None,
            "a settled tool no longer overrides background work"
        );
        assert!(
            state.is_quiet(),
            "a settled foreground tool permits replacement"
        );
    }

    #[test]
    fn completed_subagent_tool_update_keeps_a_parent_prompt_busy() {
        use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(&mut relay, "parent-prompt", prompt("keep working"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "parent-prompt"
        );

        let mut subagent = ToolCall::new("m4_csharp", "Start subagent m4_csharp");
        subagent.status = ToolCallStatus::Completed;
        relay
            .record_session_update(SessionUpdate::ToolCall(subagent))
            .unwrap();

        let state = relay.operational_state();
        assert_eq!(
            state
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.command_id.as_str()),
            Some("parent-prompt")
        );
        assert!(state.harness_turn.is_none());
        assert!(!state.is_quiet(), "the parent prompt is still in flight");

        relay
            .record_command_completed(
                "parent-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        let state = relay.operational_state();
        assert!(state.active_prompt.is_none());
        assert!(state.harness_turn.is_none());
        assert!(
            state.is_quiet(),
            "prompt completion releases the busy guard"
        );
    }

    #[test]
    fn a_restart_forgets_the_commands_the_previous_harness_left_running() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
        relay
            .record_session_update(exec_card("call-1", &["sleep", "600"], None))
            .unwrap();
        assert_eq!(relay.operational_state().background_commands.len(), 1);

        relay
            .record_observation(RelayObservation::SessionRestarted)
            .unwrap();

        assert!(
            relay.operational_state().background_commands.is_empty(),
            "the harness that owned those processes is gone"
        );
    }

    #[test]
    fn harness_turns_are_off_for_other_harnesses() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

        relay.record_session_update(tool_call_update()).unwrap();
        relay
            .record_session_update(settling_usage_update("human"))
            .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.execution, RelayExecutionState::Idle);
        assert!(state.harness_turn.is_none());
        assert!(state.last_harness_turn_started_ordinal.is_none());
        assert!(
            observations(&relay)
                .iter()
                .all(|observation| matches!(observation, RelayObservation::SessionUpdate { .. })),
            "only the updates themselves are journaled"
        );
    }

    #[test]
    fn an_in_flight_prompt_blocks_checkpoint_barrier_admission() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "stuck-prompt", prompt("keep running"));
        let prompt = relay.claim_pending_commands(true).unwrap();
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0].command_id, "stuck-prompt");

        submit_relay(
            &mut relay,
            "barrier-command",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        assert!(
            relay.claim_pending_commands(true).unwrap().is_empty(),
            "a live ACP turn must keep the checkpoint barrier queued"
        );

        relay
            .record_command_interrupted("stuck-prompt", "worker restarted")
            .unwrap();
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "barrier-command");
    }

    #[test]
    fn cancel_dispatches_while_the_prompt_it_targets_is_in_flight() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "cancelled-prompt", prompt("keep running"));
        let claimed_prompt = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed_prompt.len(), 1);
        assert_eq!(claimed_prompt[0].command_id, "cancelled-prompt");

        submit_relay(&mut relay, "queued-correction", prompt("change direction"));
        submit_relay(&mut relay, "cancel-command", RelayCommand::Cancel);
        let cancel = relay.claim_pending_commands(true).unwrap();
        assert_eq!(cancel.len(), 1);
        assert_eq!(cancel[0].command_id, "cancel-command");
        assert!(matches!(cancel[0].command, RelayCommand::Cancel));
        let steering = cancel[0]
            .steering_prompt
            .as_ref()
            .expect("cancel carries the queued prompt head");
        assert_eq!(steering.queued_command_id, "queued-correction");
        assert_eq!(
            steering.prompt,
            vec![ContentBlock::Text(
                agent_client_protocol::schema::v1::TextContent::new("change direction")
            )]
        );

        relay
            .record_command_completed(
                "cancel-command",
                RelayCommandOutcome::Steered {
                    queued_command_id: "queued-correction".into(),
                },
            )
            .unwrap();
        let state = relay.operational_state();
        assert_eq!(state.execution, RelayExecutionState::Running);
        assert_eq!(state.active_prompt.unwrap().command_id, "cancelled-prompt");
        assert!(state.queued_prompts.is_empty());

        let mut session = mj_core::state::MaterializedSession::empty(SESSION);
        for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
            let projected = mj_core::projection::project_relay_event(&session, &event).unwrap();
            mj_core::projection::apply_committed_projection_event(
                &mut session,
                &event,
                projected.mutation,
            )
            .unwrap();
        }
        assert!(matches!(
            session.execution,
            mj_core::state::MaterializedExecutionState::Running { .. }
        ));
        assert!(session.queued_prompts.is_empty());
        assert_eq!(
            session
                .transcript
                .iter()
                .filter(|item| matches!(item.body, mj_core::state::TranscriptBody::User { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn cancel_turn_bypasses_a_pending_checkpoint_without_steering_the_queue() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "active-prompt", prompt("keep running"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "active-prompt"
        );
        submit_relay(
            &mut relay,
            "pending-checkpoint",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(&mut relay, "queued-prompt", prompt("leave queued"));
        submit_relay(&mut relay, "cancel-turn", RelayCommand::CancelTurn);

        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "cancel-turn");
        assert!(matches!(claimed[0].command, RelayCommand::CancelTurn));
        assert!(claimed[0].steering_prompt.is_none());
        assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

        relay
            .record_command_completed("cancel-turn", RelayCommandOutcome::Cancelled)
            .unwrap();
        let state = relay.operational_state();
        assert_eq!(state.active_prompt.unwrap().command_id, "active-prompt");
        assert!(state.harness_turn.is_none());
        assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);
        assert!(state.checkpoint_barrier.is_none());
    }

    #[test]
    fn cancel_turn_bypasses_a_pending_checkpoint_for_an_autonomous_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay.record_session_update(tool_call_update()).unwrap();
        assert!(relay.operational_state().harness_turn.is_some());
        submit_relay(
            &mut relay,
            "pending-checkpoint",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(&mut relay, "queued-prompt", prompt("leave queued"));
        submit_relay(&mut relay, "cancel-turn", RelayCommand::CancelTurn);

        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "cancel-turn");
        assert!(claimed[0].steering_prompt.is_none());
        assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

        relay
            .record_command_completed("cancel-turn", RelayCommandOutcome::Cancelled)
            .unwrap();
        let state = relay.operational_state();
        assert!(state.harness_turn.is_some());
        assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

        relay
            .record_session_update(settling_usage_update("task-notification"))
            .unwrap();
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "pending-checkpoint");
        assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);
    }

    #[test]
    fn cancel_turn_never_bypasses_an_admitted_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "admitted-checkpoint",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "admitted-checkpoint"
        );
        relay
            .record_checkpoint_ready("admitted-checkpoint")
            .unwrap();
        let before = relay.operational_state();
        let error = relay
            .submit_command("cancel-turn", RelayCommand::CancelTurn)
            .unwrap()
            .expect_err("late cancellation must leave the checkpoint cursor intact");
        assert_eq!(error.code, RelayErrorCode::InvalidState);
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        assert_eq!(relay.operational_state(), before);
        assert!(!relay.snapshot.dispatches.contains_key("cancel-turn"));
    }

    #[test]
    fn cancel_turn_is_harmless_when_the_relay_is_idle() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(&mut relay, "cancel-idle", RelayCommand::CancelTurn);
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(claimed[0].steering_prompt.is_none());
        relay
            .record_command_completed("cancel-idle", RelayCommandOutcome::Cancelled)
            .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.execution, RelayExecutionState::Idle);
        assert!(state.active_prompt.is_none());
        assert!(state.harness_turn.is_none());
        assert!(state.is_quiet());
    }

    #[test]
    fn config_accepted_during_a_prompt_waits_while_cancel_bypasses_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "active-prompt", prompt("keep running"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "active-prompt"
        );

        submit_relay(
            &mut relay,
            "config-after-prompt",
            set_config("model", "later"),
        );
        submit_relay(&mut relay, "cancel-after-config", RelayCommand::Cancel);

        let cancel = relay.claim_pending_commands(true).unwrap();
        assert_eq!(cancel.len(), 1);
        assert_eq!(cancel[0].command_id, "cancel-after-config");
        assert!(matches!(cancel[0].command, RelayCommand::Cancel));
        assert!(cancel[0].steering_prompt.is_none());
        relay
            .record_command_completed("cancel-after-config", RelayCommandOutcome::Cancelled)
            .unwrap();
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        relay
            .record_command_completed(
                "active-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "cancelled".into(),
                    usage: None,
                },
            )
            .unwrap();
        let config = relay.claim_pending_commands(true).unwrap();
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].command_id, "config-after-prompt");
    }

    #[test]
    fn config_accepted_before_a_prompt_keeps_acceptance_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "config-first", set_config("model", "first"));
        submit_relay(&mut relay, "prompt-second", prompt("then run"));

        // Queue entries run one at a time, so the prompt waits for the
        // configuration change accepted before it.
        let config = relay.claim_pending_commands(true).unwrap();
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].command_id, "config-first");
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        relay
            .record_command_completed("config-first", RelayCommandOutcome::Configured)
            .unwrap();
        let claimed = relay.claim_pending_commands(true).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "prompt-second");
    }

    #[test]
    fn config_queued_behind_a_prompt_applies_in_queue_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "prompt-one", prompt("one"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "prompt-one"
        );

        submit_relay(&mut relay, "prompt-two", prompt("two"));
        submit_relay(&mut relay, "config-third", set_config("model", "sonnet"));
        submit_relay(&mut relay, "prompt-four", prompt("four"));
        assert_eq!(
            queued_command_ids(&relay),
            ["prompt-two", "config-third", "prompt-four"]
        );
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        finish_prompt(&mut relay, "prompt-one");
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "prompt-two"
        );
        finish_prompt(&mut relay, "prompt-two");

        let config = relay.claim_pending_commands(true).unwrap();
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].command_id, "config-third");
        assert!(matches!(config[0].command, RelayCommand::SetConfig { .. }));
        // A configuration change applies between turns, so the relay stays
        // idle and the prompt behind it waits for the change to finish.
        assert_eq!(
            relay.operational_state().execution,
            RelayExecutionState::Idle
        );
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());

        relay
            .record_command_completed("config-third", RelayCommandOutcome::Configured)
            .unwrap();
        assert_eq!(
            relay
                .operational_state()
                .config
                .get("model")
                .map(String::as_str),
            Some("sonnet")
        );
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "prompt-four"
        );
    }

    #[test]
    fn removing_a_queued_config_stops_it_from_dispatching() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "active-prompt", prompt("running"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "active-prompt"
        );
        submit_relay(&mut relay, "config-queued", set_config("effort", "high"));
        submit_relay(
            &mut relay,
            "remove-config",
            RelayCommand::RemoveQueuedPrompt {
                queued_command_id: "config-queued".into(),
            },
        );

        assert!(queued_command_ids(&relay).is_empty());
        assert_eq!(
            relay.snapshot.dispatches["config-queued"].state,
            RelayDispatchState::Rejected
        );
        assert!(
            relay.snapshot.handled_commands["config-queued"]
                .terminal_ordinal
                .is_some()
        );

        finish_prompt(&mut relay, "active-prompt");
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        assert!(relay.operational_state().config.is_empty());
    }

    #[test]
    fn clearing_the_queue_drops_queued_configuration_changes() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(&mut relay, "active-prompt", prompt("running"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "active-prompt"
        );
        submit_relay(&mut relay, "queued-prompt", prompt("later"));
        submit_relay(&mut relay, "queued-config", set_config("model", "later"));

        submit_relay(&mut relay, "clear-queue", RelayCommand::ClearQueuedPrompts);
        assert!(queued_command_ids(&relay).is_empty());

        finish_prompt(&mut relay, "active-prompt");
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    }

    #[test]
    fn an_incomplete_configuration_change_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(relay_request(
            "reject-empty-config",
            RelayRequest::Submit {
                command_id: "empty-config".into(),
                command: set_config("model", "  "),
            },
        ));
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidRequest,
                    ..
                }
            }
        ));
        assert!(queued_command_ids(&relay).is_empty());
    }

    #[test]
    fn close_requires_exact_checkpoint_cut_and_survives_controller_disconnect() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let stale_cut = ready_checkpoint(&mut relay, "stale-close-barrier");
        relay
            .record_observation(RelayObservation::Warning {
                message: "post-cut drift".into(),
            })
            .unwrap();
        let rejected = relay.handle(relay_request(
            "reject-stale-close",
            RelayRequest::Submit {
                command_id: "stale-close-command".into(),
                command: RelayCommand::Close {
                    barrier_command_id: "stale-close-barrier".into(),
                    expected: stale_cut,
                },
            },
        ));
        assert!(matches!(
            rejected.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        relay
            .cancel_checkpoint_barrier_on_disconnect("stale-close-barrier")
            .unwrap();

        let exact_cut = ready_checkpoint(&mut relay, "exact-close-barrier");
        let accepted = submit_relay(
            &mut relay,
            "exact-close-command",
            RelayCommand::Close {
                barrier_command_id: "exact-close-barrier".into(),
                expected: exact_cut.clone(),
            },
        );
        assert!(accepted > exact_cut.ordinal);
        assert_eq!(
            relay.operational_state().execution,
            RelayExecutionState::Closing
        );
        let later = relay.handle(relay_request(
            "post-close-command",
            RelayRequest::Submit {
                command_id: "post-close-prompt".into(),
                command: prompt("must not run"),
            },
        ));
        assert!(matches!(
            later.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));

        assert!(
            relay
                .cancel_checkpoint_barrier_on_disconnect("exact-close-barrier")
                .unwrap()
                .is_some()
        );
        let close = relay.claim_pending_commands(true).unwrap();
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].command_id, "exact-close-command");
        assert!(matches!(close[0].command, RelayCommand::Close { .. }));
    }

    #[test]
    fn exact_close_allows_checkpoint_completion_before_close_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let expected = ready_checkpoint(&mut relay, "normal-close-barrier");
        submit_relay(
            &mut relay,
            "normal-close-command",
            RelayCommand::Close {
                barrier_command_id: "normal-close-barrier".into(),
                expected,
            },
        );
        submit_relay(
            &mut relay,
            "normal-close-complete",
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: "normal-close-barrier".into(),
            },
        );

        let close = relay.claim_pending_commands(true).unwrap();
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].command_id, "normal-close-command");
        assert!(matches!(close[0].command, RelayCommand::Close { .. }));
    }

    #[test]
    fn credential_requests_cannot_enter_durable_relay_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

        for (request_id, request) in [
            ("credential-state", RelayRequest::CredentialState),
            ("read-credentials", RelayRequest::ReadCredentials),
            (
                "install-credentials",
                RelayRequest::InstallCredentials {
                    data: "e30=".into(),
                },
            ),
        ] {
            let response = relay.handle(relay_request(request_id, request));
            assert!(matches!(
                response.body,
                RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidState,
                        retryable: false,
                        ..
                    }
                }
            ));
        }

        assert_eq!(relay.latest_ordinal(), 0);
        assert!(
            relay
                .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
                .unwrap()
                .is_empty()
        );
        let persisted = fs::read_to_string(temp.path().join(RELAY_STATE_FILE)).unwrap();
        assert!(!persisted.contains("e30="));
        assert!(relay.snapshot.handled_commands.is_empty());
    }
}

#[cfg(test)]
mod attachment_tests;
