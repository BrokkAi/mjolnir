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
                row.session_title_override = Some(view.agent.name.clone());
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
            self.native_agents.insert(id, pane);
        }
        self.clamp_selections();
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
            KeyCode::Right | KeyCode::Enter => {
                if self.subagent_count_for(&id) > 0 {
                    self.open_subagent_workspace(id);
                }
            }
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
        id: &str,
        transcript_area: ratatui::layout::Rect,
        prompt_area: ratatui::layout::Rect,
    ) {
        use ratatui::widgets::{Block, Borders, Paragraph};
        let Some(pane) = self.native_agents.get_mut(id) else {
            return;
        };
        let title = format!(" {} · {:?} ", pane.agent.name, pane.agent.state);
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
            "Stopping…"
        } else if pane.agent.capabilities.cancel && pane.agent.state == NativeAgentState::Running {
            "s: stop · PgUp/PgDn: scroll · Enter: children · p: parent"
        } else {
            "PgUp/PgDn: scroll · Enter: children · p: parent · controlled by parent"
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
