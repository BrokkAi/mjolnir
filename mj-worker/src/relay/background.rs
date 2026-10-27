use super::*;

fn background_task_id(kind: &str, provider_id: &str) -> String {
    format!("{kind}:{provider_id}")
}

/// One adapter task and the ACP session that owns its stop operation.
#[derive(Debug, Clone)]
pub(super) struct AirTaskEntry {
    pub(super) session_id: String,
    pub(super) task_id: String,
    pub(super) state: AirTaskState,
}

#[derive(Debug, Clone)]
pub(super) enum AirTaskState {
    Background(BackgroundCommand),
    /// Retain ended turn IDs as tombstones until the adapter restarts.
    Turn(Option<u64>),
}

impl AirTaskEntry {
    pub(super) fn turn_ordinal(&self) -> Option<u64> {
        match self.state {
            AirTaskState::Turn(ordinal) => ordinal,
            _ => None,
        }
    }
    fn command(&self) -> Option<&BackgroundCommand> {
        match &self.state {
            AirTaskState::Background(command) => Some(command),
            _ => None,
        }
    }
}

fn air_task_key(session_id: &str, task_id: &str) -> String {
    format!("{}:{session_id}:{task_id}", session_id.len())
}

/// A Kimi native task together with the ACP launcher call it came from, so a
/// hosted terminal bound to that call is not listed as a second job.
#[derive(Debug, Clone)]
pub(super) struct KimiTaskEntry {
    command: BackgroundCommand,
    parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct KimiProvisionalTask {
    command: BackgroundCommand,
    native_task_id: Option<String>,
}

/// Whether this update is agent output, which is what reveals that the
/// harness is working. Everything else is session bookkeeping the harness
/// also sends while idle.
pub(super) fn is_agent_output(update: &SessionUpdate) -> bool {
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
/// into the model for that stop, so no model cycle and no result follow it.
/// Only the prefix is stable: the name that follows is whatever the adapter
/// currently calls the task, which later AIR progress messages can rename.
/// Checked against claude-agent-acp 0.81.0 `dist/async-tasks.js`
/// (`taskStopped`, via `noticeTranscriptText`) on 2026-09-22.
pub(super) const CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX: &str = "**Task stopped by user:** ";

/// The text of an agent message chunk whose content is a single text block.
pub(super) fn agent_chunk_text(update: &SessionUpdate) -> Option<&str> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => Some(&text.text),
            _ => None,
        },
        _ => None,
    }
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

impl DurableRelay {
    /// Choose where commands the agent left running are read from.
    pub fn set_background_work_policy(&mut self, policy: BackgroundWorkPolicy) {
        self.background_work = policy;
        self.background_work_known = (policy == BackgroundWorkPolicy::KimiTasks).then_some(false);
    }

    /// Commands the agent left running with nothing waiting on them, oldest
    /// first.
    ///
    /// A hosted terminal only counts while nothing of ours is in flight: until
    /// then it is the turn's own work, not something left behind. AIR tasks
    /// count as soon as their adapter announces them.
    ///
    /// Kimi is the exception to the in-flight rule: it detaches a shell into
    /// one of our terminals and keeps working, so a terminal its native journal
    /// calls detached is listed mid-turn. That terminal then represents the
    /// job, and the Kimi task entry behind it is left out.
    pub(super) fn native_agent_count(&self) -> usize {
        self.snapshot
            .native_agents
            .values()
            .filter(|agent| agent.state == mj_core::native_agent::NativeAgentState::Running)
            .count()
    }

    pub(super) fn background_commands(&self) -> Vec<BackgroundCommand> {
        let turn_in_flight = self.turn_in_progress();
        let detached_terminals = self.detached_agent_terminals();
        let mut commands: Vec<BackgroundCommand> = self
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
            .collect();
        if self.background_work == BackgroundWorkPolicy::AirTasks {
            commands.extend(
                self.air_tasks
                    .values()
                    .filter_map(|entry| entry.command().cloned()),
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

    /// Apply AIR lifecycle changes at the single owner of live background work.
    pub fn air_task_changed(
        &mut self,
        session_id: String,
        update: crate::acp::AirTaskUpdate,
    ) -> Result<()> {
        use crate::acp::AirTaskUpdate;
        if self.background_work != BackgroundWorkPolicy::AirTasks
            || matches!(
                self.snapshot.execution,
                RelayExecutionState::Closing | RelayExecutionState::Closed
            )
        {
            return Ok(());
        }
        let task_id = match &update {
            AirTaskUpdate::TurnSpawned { task_id, .. }
            | AirTaskUpdate::Spawned { task_id, .. }
            | AirTaskUpdate::Changed { task_id, .. }
            | AirTaskUpdate::Finished { task_id, .. } => task_id,
        };
        let key = air_task_key(&session_id, task_id);
        if let Some(AirTaskEntry {
            state: AirTaskState::Turn(ordinal),
            ..
        }) = self.air_tasks.get_mut(&key)
        {
            if matches!(update, AirTaskUpdate::Finished { .. }) {
                let ordinal = ordinal.take();
                // A late idle may end the next turn's task (#1453). Only a
                // silent turn trusts task end; output waits for its SDK result.
                if let Some(ordinal) = ordinal
                    && self.snapshot.active_prompt.is_none()
                    && !self.harness_turn_output_seen
                    && self
                        .snapshot
                        .harness_turn
                        .is_some_and(|turn| turn.first_ordinal == ordinal)
                {
                    self.settle_harness_turn(Some("autonomous_task_ended".into()))?;
                }
            }
            return self.persist_activity_transition();
        }
        match update {
            AirTaskUpdate::TurnSpawned { task_id, replayed } => {
                let ordinal = if !replayed
                    && self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
                    && self.snapshot.native_session_id.as_deref() == Some(session_id.as_str())
                {
                    self.open_harness_turn_if_allowed()?;
                    if self.snapshot.active_prompt.is_none() {
                        self.snapshot.harness_turn.map(|turn| turn.first_ordinal)
                    } else {
                        None
                    }
                } else {
                    None
                };
                self.air_tasks.insert(
                    key,
                    AirTaskEntry {
                        session_id,
                        task_id,
                        state: AirTaskState::Turn(ordinal),
                    },
                );
            }
            AirTaskUpdate::Spawned {
                task_id,
                description,
                can_stop,
            } => {
                let key = air_task_key(&session_id, &task_id);
                let started_at_ms = self
                    .air_tasks
                    .get(&key)
                    .and_then(AirTaskEntry::command)
                    .map_or_else(epoch_millis, |command| command.started_at_ms);
                self.air_tasks.insert(
                    key.clone(),
                    AirTaskEntry {
                        session_id,
                        task_id,
                        state: AirTaskState::Background(BackgroundCommand {
                            id: background_task_id("air", &key),
                            started_at_ms,
                            command: description,
                            can_stop,
                        }),
                    },
                );
            }
            AirTaskUpdate::Changed {
                task_id,
                description,
                can_stop,
            } => {
                if let Some(AirTaskEntry {
                    state: AirTaskState::Background(command),
                    ..
                }) = self.air_tasks.get_mut(&air_task_key(&session_id, &task_id))
                {
                    if let Some(description) = description {
                        command.command = description;
                    }
                    if let Some(can_stop) = can_stop {
                        command.can_stop = can_stop;
                    }
                }
            }
            AirTaskUpdate::Finished { task_id, settled } => {
                let removed = self.air_tasks.remove(&air_task_key(&session_id, &task_id));
                if removed.is_some()
                    && settled
                    && self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
                    && self.snapshot.native_agent_replay.is_none()
                {
                    self.note_task_settled();
                }
            }
        }
        self.persist_activity_transition()
    }

    /// Replace Kimi's native task level and reconcile ACP launch or query
    /// evidence against task and launcher identities retained by the wire.
    pub fn kimi_native_agents_changed(
        &mut self,
        agents: &BTreeMap<
            String,
            (
                crate::acp::KimiBackgroundTask,
                mj_core::native_agent::NativeAgentState,
            ),
        >,
    ) -> Result<()> {
        use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent};
        let missing: Vec<_> = self
            .snapshot
            .native_agents
            .iter()
            .filter(|(id, agent)| {
                !agents.contains_key(*id)
                    && agent.state == mj_core::native_agent::NativeAgentState::Running
            })
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in missing {
            self.record_observation(RelayObservation::NativeAgent {
                event: NativeAgentEvent::State {
                    session_id,
                    state: mj_core::native_agent::NativeAgentState::Disconnected,
                },
            })?;
        }
        for (id, (task, state)) in agents {
            if !self.snapshot.native_agents.contains_key(id) {
                self.record_observation(RelayObservation::NativeAgent {
                    event: NativeAgentEvent::Spawned {
                        session_id: id.clone(),
                        parent_session_id: None,
                        name: task.description.clone(),
                        task: task.description.clone(),
                        capabilities: NativeAgentCapabilities::default(),
                    },
                })?;
            }
            if self
                .snapshot
                .native_agents
                .get(id)
                .is_some_and(|agent| agent.state != *state)
            {
                self.record_observation(RelayObservation::NativeAgent {
                    event: NativeAgentEvent::State {
                        session_id: id.clone(),
                        state: *state,
                    },
                })?;
            }
        }
        Ok(())
    }

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
            .filter(|task| !task.is_agent)
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

    /// A background task the harness will answer with a turn of its own just
    /// settled. Until that turn opens, the session is not quiet.
    pub(super) fn note_task_settled(&mut self) {
        if self.snapshot.active_prompt.is_none() && self.snapshot.harness_turn.is_none() {
            self.task_settled_at_ms = Some(epoch_millis());
        }
    }

    /// Resolve a public task id only while it still names stoppable background
    /// work in this process.
    pub fn background_task_stop_target(
        &mut self,
        requested_id: &str,
    ) -> Result<BackgroundTaskStopTarget> {
        if let Some(child) = requested_id.strip_prefix("native-agent:") {
            let agent = self
                .snapshot
                .native_agents
                .get(child)
                .filter(|agent| {
                    agent.state == mj_core::native_agent::NativeAgentState::Running
                        && agent.capabilities.cancel
                })
                .ok_or_else(|| anyhow!("native agent is no longer cancellable"))?;
            return Ok(BackgroundTaskStopTarget::NativeAgent {
                session_id: agent.session_id.clone(),
            });
        }
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
        if let Some((key, entry)) = self.air_tasks.iter().find(|(_, entry)| {
            entry
                .command()
                .is_some_and(|command| command.id == requested_id)
        }) {
            if self.harness_turns == HarnessTurnPolicy::ClaudeAdapter {
                self.claude_pending_stops
                    .entry(entry.session_id.clone())
                    .or_default()
                    .insert(key.clone());
            }
            return Ok(BackgroundTaskStopTarget::AirAsyncTask {
                session_id: entry.session_id.clone(),
                task_id: entry.task_id.clone(),
            });
        }
        bail!("background task is no longer running")
    }

    /// Forget a Claude stop that never reached the adapter, so a later chunk
    /// with the acknowledgement prefix is not mistaken for an answer to it.
    pub fn claude_stop_not_sent(&mut self, session_id: &str, task_id: &str) {
        if let Some(pending) = self.claude_pending_stops.get_mut(session_id) {
            pending.remove(&air_task_key(session_id, task_id));
            if pending.is_empty() {
                self.claude_pending_stops.remove(session_id);
            }
        }
    }

    /// Whether this update is the adapter acknowledging a stop this process
    /// requested: a single text block with the acknowledgement prefix while a
    /// stop is pending. The adapter names no task id in the chunk and may
    /// have renamed the task since its spawn, so the pairing is
    /// by count, not by name.
    fn is_claude_stop_acknowledgement_for(
        &self,
        session_id: Option<&str>,
        update: &SessionUpdate,
    ) -> bool {
        self.harness_turns == HarnessTurnPolicy::ClaudeAdapter
            && session_id.is_some_and(|id| {
                self.claude_pending_stops
                    .get(id)
                    .is_some_and(|pending| !pending.is_empty())
            })
            && agent_chunk_text(update)
                .is_some_and(|text| text.starts_with(CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX))
    }

    pub(super) fn is_claude_stop_acknowledgement(&self, update: &SessionUpdate) -> bool {
        self.is_claude_stop_acknowledgement_for(self.snapshot.native_session_id.as_deref(), update)
    }

    pub(super) fn consume_claude_stop_acknowledgement(
        &mut self,
        session_id: Option<&str>,
        update: &SessionUpdate,
    ) -> bool {
        if !self.is_claude_stop_acknowledgement_for(session_id, update) {
            return false;
        }
        let Some(session_id) = session_id else {
            return false;
        };
        let Some(pending) = self.claude_pending_stops.get_mut(session_id) else {
            return false;
        };
        let consumed = pending.pop_first().is_some();
        if pending.is_empty() {
            self.claude_pending_stops.remove(session_id);
        }
        consumed
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
            .close(&crate::acp::fallback_terminal_tool_call_id(terminal_id));
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
    pub(super) fn forget_harness_processes(&mut self) {
        self.air_tasks.clear();
        self.claude_pending_stops.clear();
        self.harness_turn_output_seen = false;
        self.task_settled_at_ms = None;
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

    /// Remember which hosted terminal a tool call embedded. Kimi launches a
    /// detached shell through a terminal of ours and reports the terminal id
    /// on the launcher card, which is the only link between the ACP card and
    /// the process its native journal later names.
    pub(super) fn track_agent_terminal_tool_call(&mut self, update: &SessionUpdate) {
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
    pub(super) fn track_kimi_background_agent(&mut self, update: &SessionUpdate) {
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
}

/// Fixtures are applied as AIR spawn/terminal edges, never as a provider level.
#[cfg(test)]
pub(crate) struct AirTaskFixture {
    pub task_id: String,
    pub description: String,
}

#[cfg(test)]
impl DurableRelay {
    pub(crate) fn set_air_tasks(&mut self, tasks: Vec<AirTaskFixture>) -> Result<()> {
        use crate::acp::AirTaskUpdate;
        let ended: Vec<_> = self
            .air_tasks
            .values()
            .filter(|entry| {
                entry.session_id == "root"
                    && !tasks.iter().any(|task| task.task_id == entry.task_id)
            })
            .map(|entry| entry.task_id.clone())
            .collect();
        for task_id in ended {
            self.air_task_changed(
                "root".into(),
                AirTaskUpdate::Finished {
                    task_id,
                    settled: false,
                },
            )?;
        }
        for task in tasks {
            let can_stop = self
                .air_tasks
                .get(&air_task_key("root", &task.task_id))
                .and_then(AirTaskEntry::command)
                .is_some_and(|command| command.can_stop);
            self.air_task_changed(
                "root".into(),
                AirTaskUpdate::Spawned {
                    task_id: task.task_id,
                    description: task.description,
                    can_stop,
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl DurableRelay {
    pub(crate) fn apply_claude_task_fixture(
        &mut self,
        task_id: String,
        can_stop: bool,
        autonomous_turn: bool,
        settled: bool,
        replayed: bool,
    ) -> Result<()> {
        use crate::acp::AirTaskUpdate;
        let update = if autonomous_turn {
            AirTaskUpdate::TurnSpawned { task_id, replayed }
        } else if can_stop {
            AirTaskUpdate::Changed {
                task_id,
                description: None,
                can_stop: Some(true),
            }
        } else {
            AirTaskUpdate::Finished { task_id, settled }
        };
        let session = self
            .snapshot
            .native_session_id
            .clone()
            .unwrap_or_else(|| "root".into());
        self.air_task_changed(session, update)
    }
    pub(crate) fn turn_task_ordinal(&self, task_id: &str) -> Option<u64> {
        let session = self.snapshot.native_session_id.as_deref().unwrap_or("root");
        self.air_tasks
            .get(&air_task_key(session, task_id))
            .and_then(AirTaskEntry::turn_ordinal)
    }
}
