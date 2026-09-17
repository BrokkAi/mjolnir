use super::*;

fn background_task_id(kind: &str, provider_id: &str) -> String {
    format!("{kind}:{provider_id}")
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

/// `_meta` key the Claude adapter puts on the `usage_update` that settles a
/// turn. Its value is an object with a `kind` naming the origin.
pub(super) const CLAUDE_ORIGIN_META_KEY: &str = "_claude/origin";

/// The origin kind on a settling `usage_update`, if the marker is present.
///
/// The outer option says whether the update carries the marker at all; the
/// inner one is the origin kind, which is only ever reported for diagnostics.
pub(super) fn claude_turn_origin(update: &SessionUpdate) -> Option<Option<String>> {
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
/// into the model for that stop, so no turn and no origin marker follow it.
/// Only the prefix is stable: the name that follows is whatever the adapter
/// currently calls the task, which later level and start messages rename.
/// Checked against claude-agent-acp 0.73.0 `dist/async-tasks.js`
/// (`taskStopped`) on 2026-09-14.
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
    /// then it is the turn's own work, not something left behind. A Codex exec
    /// card counts from the moment its result arrives with a null exit code,
    /// because Codex starts these during a turn and never mentions them again.
    ///
    /// Kimi is the exception to the in-flight rule: it detaches a shell into
    /// one of our terminals and keeps working, so a terminal its native journal
    /// calls detached is listed mid-turn. That terminal then represents the
    /// job, and the Kimi task entry behind it is left out.
    pub(super) fn background_commands(&self) -> Vec<BackgroundCommand> {
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
    pub(super) fn is_claude_stop_acknowledgement(&self, update: &SessionUpdate) -> bool {
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
    pub(super) fn forget_harness_processes(&mut self) {
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

    /// Track tool statuses that prove the agent is still doing foreground
    /// work. Pending and in-progress have portable ACP meanings across every
    /// harness; prose and plan updates do not carry a corresponding end, so
    /// they cannot safely override known background work on their own.
    pub(super) fn track_foreground_tool(&mut self, update: &SessionUpdate) {
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
    pub(super) fn track_codex_exec_card(&mut self, update: &SessionUpdate) {
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
