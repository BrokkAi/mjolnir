use super::*;
use mj_core::native_agent::{NativeAgent, NativeAgentState, NativeAgentView};

pub(crate) struct NativeAgentPane {
    pub agent: NativeAgent,
    transcript: mj_chat::chat::TranscriptSnapshot,
    projection: mj_core::state::MaterializedSession,
    generation: u64,
    loading_history: bool,
    has_more: bool,
    scroll: usize,
    pub stopping: bool,
}

impl DashboardState {
    pub fn is_native_agent(&self, id: &str) -> bool {
        self.native_agents.contains_key(id)
    }

    pub(crate) fn subagent_parent_for(&self, id: &str) -> Option<String> {
        self.native_agents
            .get(id)
            .map(|pane| pane.agent.parent_view_id())
            .or_else(|| {
                self.state
                    .subagents
                    .get(id)
                    .map(|agent| agent.parent_session_id.clone())
            })
    }

    pub fn working_subagent_count_for(&self, parent: &str) -> usize {
        let managed = self
            .state
            .subagents
            .values()
            .filter(|a| a.parent_session_id == parent)
            .filter(|a| {
                self.session_details
                    .get(&a.child_session_id)
                    .is_some_and(|d| {
                        d.activity
                            .is_working(d.current_turn_started_at, d.awaiting_input)
                    })
            })
            .count();
        let native: BTreeSet<_> = self
            .native_agents
            .values()
            .filter(|p| {
                p.agent.parent_view_id() == parent && p.agent.state == NativeAgentState::Running
            })
            .map(|p| p.agent.stable_id.as_ref().unwrap_or(&p.agent.session_id))
            .collect();
        managed + native.len()
    }

    pub fn subagent_count_for(&self, parent: &str) -> usize {
        self.state
            .subagents
            .values()
            .filter(|agent| agent.parent_session_id == parent)
            .count()
            + self
                .native_agents
                .values()
                .filter(|pane| pane.agent.parent_view_id() == parent)
                .count()
    }

    pub fn set_native_agents(&mut self, views: Vec<NativeAgentView>) {
        let ids: BTreeSet<_> = views.iter().map(|view| view.agent.view_id()).collect();
        for id in self.native_agents.keys().filter(|id| !ids.contains(*id)) {
            self.state.sessions.remove(id);
        }
        self.native_agents.retain(|id, _| ids.contains(id));
        for view in views {
            let id = view.agent.view_id();
            // These records exist only in the presentation model. The controller
            // never sees a native child as a provisionable session.
            if let Some(mut row) = self
                .state
                .sessions
                .get(&view.agent.owner_session_id)
                .cloned()
            {
                row.id = id.clone();
                row.title = view.agent.name.clone();
                row.session_title_override = Some(format!(
                    "{} · {}",
                    view.agent.name,
                    view.agent.lifecycle_label()
                ));
                row.acp_session_title = None;
                row.state = if view.agent.state == NativeAgentState::Running {
                    SessionState::Running
                } else {
                    SessionState::Stopped
                };
                row.last_error = None;
                self.state.sessions.insert(id.clone(), row);
            }
            let previous = self
                .native_agents
                .remove(&id)
                .filter(|pane| pane.generation == view.generation_ordinal);
            let mut pane = if let Some(mut previous) = previous {
                if previous.projection.applied_event_ordinal
                    != view.projection.applied_event_ordinal
                {
                    let old_items = std::mem::take(&mut previous.projection.transcript);
                    previous.projection = view.projection;
                    merge_native_items(&mut previous.projection, old_items);
                    previous.transcript =
                        mj_chat::chat::TranscriptSnapshot::from_materialized(&previous.projection);
                }
                previous.agent = view.agent;
                previous
            } else {
                NativeAgentPane {
                    transcript: mj_chat::chat::TranscriptSnapshot::from_materialized(
                        &view.projection,
                    ),
                    projection: view.projection,
                    generation: view.generation_ordinal,
                    agent: view.agent,
                    scroll: 0,
                    stopping: false,
                    loading_history: false,
                    has_more: true,
                }
            };
            pane.stopping &= pane.agent.state == NativeAgentState::Running;
            self.set_native_row_excerpt(&id, &pane.projection.transcript);
            self.native_agents.insert(id, pane);
        }
        self.clamp_selections();
    }

    /// Gives a native child's row the excerpt other rows get from their
    /// stored summary. A native child has no summary of its own, so without
    /// this its row read "No messages yet" whatever it had said (R10-2).
    fn set_native_row_excerpt(
        &mut self,
        id: &str,
        transcript: &[std::sync::Arc<mj_core::state::TranscriptItem>],
    ) {
        let none = crate::ingest::MaterializedProjectionCache::default();
        let detail = self.session_details.entry(id.to_owned()).or_default();
        detail.last_agent_message =
            crate::ingest::last_agent_message(transcript, 0, &none).map(|(_, text)| text);
        detail.latest_agent_activity_after_last_user =
            crate::ingest::latest_agent_activity(transcript, 0, &none).map(|(_, text)| text);
        detail.last_agent_message_follows_last_user = detail.last_agent_message.is_some();
    }

    pub fn native_agent_history_loaded(
        &mut self,
        owner: &str,
        child: &str,
        result: Result<mj_core::native_agent::NativeAgentHistoryPage, String>,
    ) {
        let Some(pane) = self
            .native_agents
            .get_mut(&mj_core::native_agent::view_id(owner, child))
        else {
            return;
        };
        pane.loading_history = false;
        match result {
            Ok(page) if page.generation_ordinal == pane.generation => {
                pane.has_more = page.has_more;
                merge_native_items(&mut pane.projection, page.items);
                pane.transcript =
                    mj_chat::chat::TranscriptSnapshot::from_materialized(&pane.projection);
            }
            Ok(_) => {}
            Err(error) => self.set_notice(format!("Could not load native agent history: {error}")),
        }
    }

    pub fn native_agent_stop_finished(
        &mut self,
        owner: &str,
        child: &str,
        result: Result<(), String>,
    ) {
        if let Some(pane) = self
            .native_agents
            .get_mut(&mj_core::native_agent::view_id(owner, child))
            && result.is_err()
        {
            pane.stopping = false;
        }
        if let Err(error) = result {
            self.set_notice(format!("Could not stop native agent: {error}"));
        }
    }

    pub(crate) fn native_agent_key(&mut self, key: KeyEvent) -> Option<DashboardAction> {
        let id = match self.focus {
            Focus::Sessions if self.sessions_filter.is_none() => self.selected_session_id()?,
            Focus::Prompt => self.current_session_id()?,
            _ => return None,
        }
        .to_owned();
        let has_children = self.subagent_count_for(&id) > 0;
        let pane = self.native_agents.get_mut(&id)?;
        match key.code {
            KeyCode::PageUp => {
                pane.scroll = pane.scroll.saturating_add(20);
                if !pane.loading_history && pane.has_more {
                    pane.loading_history = true;
                    return Some(DashboardAction::LoadNativeAgentHistory {
                        owner: pane.agent.owner_session_id.clone(),
                        child: pane.agent.session_id.clone(),
                        before: pane
                            .projection
                            .transcript
                            .first()
                            .map(|item| (item.position, item.stable_id.clone())),
                    });
                }
            }
            KeyCode::PageDown => pane.scroll = pane.scroll.saturating_sub(20),
            KeyCode::End => pane.scroll = 0,
            KeyCode::Char('p') if key.modifiers.is_empty() => {
                let parent = pane.agent.parent_view_id();
                self.subagent_parent_id = self.subagent_parent_for(&parent);
                self.selected_session_id = Some(parent.clone());
                return Some(DashboardAction::Open { session_id: parent });
            }
            KeyCode::Right | KeyCode::Enter if has_children => {
                self.open_subagent_workspace(id);
            }
            // With no children of its own, Enter on the row opens the child's
            // conversation, as it does for any other row (R10-2).
            KeyCode::Enter if self.focus == Focus::Sessions => {
                return Some(DashboardAction::Open { session_id: id });
            }
            KeyCode::Right | KeyCode::Enter => {}
            KeyCode::Char('s')
                if key.modifiers.is_empty()
                    && pane.agent.capabilities.cancel
                    && pane.agent.state == NativeAgentState::Running
                    && !pane.stopping =>
            {
                pane.stopping = true;
                return Some(DashboardAction::StopNativeAgent {
                    owner: pane.agent.owner_session_id.clone(),
                    child: pane.agent.session_id.clone(),
                });
            }
            _ => return None,
        }
        Some(DashboardAction::None)
    }

    pub(crate) fn render_native_agent(
        &mut self,
        frame: &mut ratatui::Frame,
        pane_id: crate::tile_layout::PaneId,
        id: &str,
        transcript_area: ratatui::layout::Rect,
        prompt_area: ratatui::layout::Rect,
    ) {
        use ratatui::widgets::{Block, Borders, Paragraph};
        let children = if self.subagent_count_for(id) > 0 {
            " · Enter: children"
        } else {
            ""
        };
        let Some(agent) = self.native_agents.get(id).map(|pane| &pane.agent) else {
            return;
        };
        let title = crate::pane_controls::child_pane_title(
            self,
            pane_id,
            transcript_area.width,
            &agent.name,
            &[agent.activity_label(), agent.availability.label()],
        );
        let Some(pane) = self.native_agents.get_mut(id) else {
            return;
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(transcript_area);
        frame.render_widget(block, transcript_area);
        let (lines, scroll) =
            pane.transcript
                .rich_tail_scrolled(inner.width, inner.height as usize, pane.scroll);
        pane.scroll = scroll;
        if pane.projection.transcript.is_empty() {
            frame.render_widget(Paragraph::new("No child transcript available yet."), inner);
        } else {
            frame.render_widget(Paragraph::new(lines), inner);
        }
        let controls = if pane.stopping {
            "Suspending…".to_owned()
        } else if pane.agent.capabilities.cancel && pane.agent.state == NativeAgentState::Running {
            format!("s: stop · PgUp/PgDn: scroll{children} · p: parent")
        } else {
            format!("PgUp/PgDn: scroll{children} · p: parent · controlled by parent")
        };
        frame.render_widget(
            Paragraph::new(format!("{}\n{}", pane.agent.task, controls))
                .block(Block::default().borders(Borders::ALL).title("Native agent")),
            prompt_area,
        );
    }
}

fn merge_native_items(
    projection: &mut mj_core::state::MaterializedSession,
    older: Vec<std::sync::Arc<mj_core::state::TranscriptItem>>,
) {
    let mut items: BTreeMap<_, _> = older
        .into_iter()
        .map(|item| (item.stable_id.clone(), item))
        .collect();
    for item in projection.transcript.drain(..) {
        items.insert(item.stable_id.clone(), item);
    }
    projection.transcript = items.into_values().collect();
    projection
        .transcript
        .sort_by(|a, b| (a.position, &a.stable_id).cmp(&(b.position, &b.stable_id)));
}
