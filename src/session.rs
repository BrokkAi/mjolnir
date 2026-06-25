//! Session management: listing and resuming ACP sessions.
//!
//! Provides both headless listing (`mj resume --list`) and interactive
//! session picking (`mj resume` without arguments). Sessions are listed
//! by spawning the agent, initializing ACP, calling `session/list`, and
//! collecting results before entering the TUI.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::term::TrackedBackend;
use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, DeleteSessionRequest, ErrorCode,
    Implementation, InitializeRequest, ListSessionsRequest, ProtocolVersion, SessionInfo,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use serde::Serialize;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use unicode_width::UnicodeWidthStr;

use crate::acp;
use crate::config::SelectedAgent;
use crate::version::mjolnir_version_label;

/// One row in the session picker.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

impl From<SessionInfo> for SessionEntry {
    fn from(info: SessionInfo) -> Self {
        Self {
            session_id: info.session_id.to_string(),
            cwd: info.cwd,
            title: info.title,
            updated_at: info.updated_at,
        }
    }
}

/// Serializable session info for `mj resume --list --format json`.
#[derive(Debug, Serialize)]
pub struct SessionEntryJson {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl From<&SessionEntry> for SessionEntryJson {
    fn from(e: &SessionEntry) -> Self {
        Self {
            session_id: e.session_id.clone(),
            cwd: e.cwd.display().to_string(),
            title: e.title.clone(),
            updated_at: e.updated_at.clone(),
        }
    }
}

/// Sessions and related capabilities advertised by the agent.
#[derive(Debug, Clone)]
pub struct SessionListResult {
    pub sessions: Vec<SessionEntry>,
    pub delete_supported: bool,
}

/// Outcome of the interactive session picker.
#[derive(Debug)]
pub enum ResumeOutcome {
    /// User selected a session to resume.
    Selected(SessionEntry),
    /// User confirmed a request to delete a session.
    DeleteRequested(SessionEntry),
    /// User cancelled with Esc.
    Cancelled,
}

/// List sessions from the configured agent without entering the TUI.
pub async fn list_sessions(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
) -> Result<Vec<SessionEntry>> {
    Ok(list_sessions_with_capabilities(agent, cwd, agent_stderr)
        .await?
        .sessions)
}

/// List sessions and return the session management capabilities advertised by the agent.
pub async fn list_sessions_with_capabilities(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
) -> Result<SessionListResult> {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let prepared = acp::prepare_agent_command_for_spawn(&agent.program, &agent.env, &ui_tx)
        .await
        .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
        .context("prepare agent for session listing")?;

    let (mut child, child_stdin, child_stdout) =
        acp::spawn_agent(&prepared.command, &agent.args, &prepared.env, agent_stderr)
            .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
            .context("spawn agent for session listing")?;
    let agent_pid = child.id();

    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    let sessions = list_sessions_via_transport(transport, cwd).await;

    acp::kill_agent_tree(&mut child, agent_pid).await;

    sessions
}

/// Delete a session through the configured agent.
pub async fn delete_session(
    agent: &SelectedAgent,
    session_id: String,
    agent_stderr: Option<&Path>,
) -> Result<()> {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let prepared = acp::prepare_agent_command_for_spawn(&agent.program, &agent.env, &ui_tx)
        .await
        .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
        .context("prepare agent for session deletion")?;

    let (mut child, child_stdin, child_stdout) =
        acp::spawn_agent(&prepared.command, &agent.args, &prepared.env, agent_stderr)
            .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
            .context("spawn agent for session deletion")?;
    let agent_pid = child.id();

    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());
    let result = delete_session_via_transport(transport, session_id).await;

    acp::kill_agent_tree(&mut child, agent_pid).await;

    result
}

/// Drive the ACP client to list sessions over an existing transport.
async fn list_sessions_via_transport<T>(transport: T, cwd: PathBuf) -> Result<SessionListResult>
where
    T: ConnectTo<Client>,
{
    let result = Client
        .builder()
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            // Initialize handshake.
            let init_req =
                InitializeRequest::new(ProtocolVersion::V1).client_info(client_implementation());
            let init_resp = conn
                .send_request(init_req)
                .block_task()
                .await
                .context("initialize for session listing")?;
            validate_protocol_version(init_resp.protocol_version)?;
            require_session_list(&init_resp.agent_capabilities)?;
            let delete_supported = session_delete_supported(&init_resp.agent_capabilities);

            // Collect all pages of sessions.
            let mut all_sessions: Vec<SessionEntry> = Vec::new();
            let mut cursor: Option<String> = None;
            let mut attempted_auth = false;
            loop {
                let mut list_req = ListSessionsRequest::new().cwd(cwd.clone());
                list_req.cursor = cursor.clone();
                let resp = match conn.send_request(list_req.clone()).block_task().await {
                    Ok(resp) => resp,
                    Err(err) if is_auth_required(&err) && !attempted_auth => {
                        authenticate_with_first_method(&conn, &init_resp.auth_methods).await?;
                        attempted_auth = true;
                        conn.send_request(list_req).block_task().await?
                    }
                    Err(err) => return Err(err),
                };
                all_sessions.extend(resp.sessions.into_iter().map(SessionEntry::from));
                match resp.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }

            Ok(SessionListResult {
                sessions: all_sessions,
                delete_supported,
            })
        })
        .await;

    result.context("ACP client error during session listing")
}

async fn delete_session_via_transport<T>(transport: T, session_id: String) -> Result<()>
where
    T: ConnectTo<Client>,
{
    let result = Client
        .builder()
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            let init_req =
                InitializeRequest::new(ProtocolVersion::V1).client_info(client_implementation());
            let init_resp = conn
                .send_request(init_req)
                .block_task()
                .await
                .context("initialize for session deletion")?;
            validate_protocol_version(init_resp.protocol_version)?;
            require_session_delete(&init_resp.agent_capabilities)?;

            let delete_req = DeleteSessionRequest::new(session_id);
            match conn.send_request(delete_req.clone()).block_task().await {
                Ok(_) => Ok(()),
                Err(err) if is_auth_required(&err) => {
                    authenticate_with_first_method(&conn, &init_resp.auth_methods).await?;
                    conn.send_request(delete_req).block_task().await?;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        })
        .await;

    result.context("ACP client error during session deletion")
}

async fn authenticate_with_first_method(
    conn: &ConnectionTo<Agent>,
    auth_methods: &[AuthMethod],
) -> std::result::Result<(), agent_client_protocol::Error> {
    let Some(method) = auth_methods.first() else {
        return Err(
            agent_client_protocol::Error::auth_required().data(serde_json::Value::String(
                "agent requires authentication but did not advertise any ACP auth methods"
                    .to_string(),
            )),
        );
    };
    conn.send_request(AuthenticateRequest::new(method.id().clone()))
        .block_task()
        .await?;
    Ok(())
}

fn client_implementation() -> Implementation {
    Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).title("Mjolnir")
}

fn is_auth_required(err: &agent_client_protocol::Error) -> bool {
    err.code == ErrorCode::AuthRequired
}

fn validate_protocol_version(negotiated: ProtocolVersion) -> Result<()> {
    if negotiated == ProtocolVersion::LATEST {
        Ok(())
    } else {
        anyhow::bail!(
            "agent negotiated unsupported ACP protocol version {negotiated}; mjolnir supports ACP {}",
            ProtocolVersion::LATEST
        )
    }
}

fn require_session_list(capabilities: &AgentCapabilities) -> Result<()> {
    if capabilities.session_capabilities.list.is_some() {
        Ok(())
    } else {
        anyhow::bail!("agent does not advertise ACP capability sessionCapabilities.list")
    }
}

fn session_delete_supported(capabilities: &AgentCapabilities) -> bool {
    capabilities.session_capabilities.delete.is_some()
}

fn require_session_delete(capabilities: &AgentCapabilities) -> Result<()> {
    if session_delete_supported(capabilities) {
        Ok(())
    } else {
        anyhow::bail!("agent does not advertise ACP capability sessionCapabilities.delete")
    }
}

/// Interactive session picker state.
struct SessionPickerState {
    sessions: Vec<SessionEntry>,
    filter: String,
    filtered: Vec<usize>,
    selected: usize,
    delete_supported: bool,
    confirming_delete: Option<String>,
    notice: Option<String>,
    notice_scroll: u16,
}

impl SessionPickerState {
    fn new(sessions: Vec<SessionEntry>, delete_supported: bool, notice: Option<String>) -> Self {
        let mut state = Self {
            sessions,
            filter: String::new(),
            filtered: Vec::new(),
            selected: 0,
            delete_supported,
            confirming_delete: None,
            notice,
            notice_scroll: 0,
        };
        state.recompute_filter();
        state
    }

    fn recompute_filter(&mut self) {
        let q = self.filter.to_lowercase();
        let prev_selected_id = self
            .filtered
            .get(self.selected)
            .map(|&i| self.sessions[i].session_id.clone());

        if q.is_empty() {
            self.filtered = (0..self.sessions.len()).collect();
        } else {
            self.filtered = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.session_id.to_lowercase().contains(&q)
                        || s.title
                            .as_deref()
                            .map(|t| t.to_lowercase().contains(&q))
                            .unwrap_or(false)
                        || s.cwd.to_string_lossy().to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
        }

        // Preserve selection on the same row when possible; otherwise top.
        self.selected = prev_selected_id
            .and_then(|id| {
                self.filtered
                    .iter()
                    .position(|&i| self.sessions[i].session_id == id)
            })
            .unwrap_or(0);
    }

    fn move_selection(&mut self, delta: i32) {
        self.confirming_delete = None;
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as i32;
        self.selected = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn focused_session(&self) -> Option<&SessionEntry> {
        self.filtered.get(self.selected).map(|&i| &self.sessions[i])
    }

    fn delete_confirmation_session(&self) -> Option<&SessionEntry> {
        let id = self.confirming_delete.as_ref()?;
        self.sessions
            .iter()
            .find(|session| &session.session_id == id)
    }

    fn request_delete_confirmation(&mut self) {
        if !self.delete_supported {
            return;
        }
        self.confirming_delete = self
            .focused_session()
            .map(|session| session.session_id.clone());
        self.notice = None;
        self.notice_scroll = 0;
    }

    fn cancel_delete_confirmation(&mut self) {
        self.confirming_delete = None;
    }

    fn scroll_notice(&mut self, delta: i32) {
        if self.notice.is_none() && self.confirming_delete.is_none() {
            return;
        }
        if delta.is_negative() {
            self.notice_scroll = self
                .notice_scroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            self.notice_scroll = self.notice_scroll.saturating_add(delta as u16);
        }
    }
}

/// Run the interactive session picker until the user selects or cancels.
pub async fn run_session_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sessions: Vec<SessionEntry>,
    delete_supported: bool,
    notice: Option<String>,
) -> Result<ResumeOutcome> {
    let mut state = SessionPickerState::new(sessions, delete_supported, notice);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw_session_picker(f, &state))?;

    loop {
        tokio::select! {
            biased;
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else {
                    return Ok(ResumeOutcome::Cancelled);
                };
                let ev = ev.context("crossterm event stream")?;
                if let Some(outcome) = handle_session_picker_event(&mut state, ev) {
                    return Ok(outcome);
                }
            }
            _ = tick.tick() => {}
        }
        terminal.draw(|f| draw_session_picker(f, &state))?;
    }
}

fn handle_session_picker_event(
    state: &mut SessionPickerState,
    ev: CtEvent,
) -> Option<ResumeOutcome> {
    let CtEvent::Key(key) = ev else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }

    if state.confirming_delete.is_some() {
        return match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => Some(ResumeOutcome::Cancelled),
            (_, KeyCode::Esc) | (_, KeyCode::Char('n') | KeyCode::Char('N')) => {
                state.cancel_delete_confirmation();
                None
            }
            (_, KeyCode::PageUp) => {
                state.scroll_notice(-3);
                None
            }
            (_, KeyCode::PageDown) => {
                state.scroll_notice(3);
                None
            }
            (_, KeyCode::Char('y') | KeyCode::Char('Y')) => state
                .delete_confirmation_session()
                .cloned()
                .map(ResumeOutcome::DeleteRequested),
            _ => None,
        };
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => {
            Some(ResumeOutcome::Cancelled)
        }
        (_, KeyCode::Up) => {
            state.move_selection(-1);
            None
        }
        (_, KeyCode::Down) => {
            state.move_selection(1);
            None
        }
        (_, KeyCode::PageUp) => {
            state.scroll_notice(-3);
            None
        }
        (_, KeyCode::PageDown) => {
            state.scroll_notice(3);
            None
        }
        (_, KeyCode::Enter) => state
            .focused_session()
            .cloned()
            .map(ResumeOutcome::Selected),
        (_, KeyCode::Delete) => {
            state.request_delete_confirmation();
            None
        }
        (_, KeyCode::Backspace) => {
            state.cancel_delete_confirmation();
            state.filter.pop();
            state.recompute_filter();
            None
        }
        (_, KeyCode::Char(c)) => {
            state.cancel_delete_confirmation();
            state.filter.push(c);
            state.recompute_filter();
            None
        }
        _ => None,
    }
}

fn draw_session_picker(f: &mut ratatui::Frame, state: &SessionPickerState) {
    let notice_text = session_picker_notice_text(state);
    let notice_height = session_picker_notice_height(f.area(), notice_text.as_deref());
    let notice_scrollable = notice_needs_scroll(f.area(), notice_text.as_deref(), notice_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(notice_height),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Header
    let header = Paragraph::new(format!(" {} | resume a session ", mjolnir_version_label()))
        .style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(header, chunks[0]);

    // Session list
    let block = Block::default().borders(Borders::ALL).title(" sessions ");
    let inner = block.inner(chunks[1]);
    f.render_widget(block, chunks[1]);

    if state.filtered.is_empty() {
        let p = Paragraph::new("no sessions found").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
    } else {
        let visible = inner.height as usize;
        let total = state.filtered.len();
        let start = if total <= visible {
            0
        } else {
            let half = visible / 2;
            state.selected.saturating_sub(half).min(total - visible)
        };
        let end = (start + visible).min(total);

        let items: Vec<ListItem> = state.filtered[start..end]
            .iter()
            .enumerate()
            .map(|(offset, &i)| {
                let absolute = start + offset;
                let session = &state.sessions[i];
                let marker = if absolute == state.selected { ">" } else { " " };

                // Build label: title or session ID.
                let label = session.title.as_deref().unwrap_or(&session.session_id);

                // Build hint: cwd + updated_at.
                let mut hint_parts = vec![session.cwd.to_string_lossy().to_string()];
                if let Some(updated) = &session.updated_at {
                    hint_parts.push(updated.clone());
                }
                let hint = hint_parts.join("  --  ");

                let line = format!("{marker} {label}  -- {hint}");
                let style = if absolute == state.selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect();

        let list = List::new(items);
        f.render_widget(list, inner);
    }

    // Notice / confirmation
    if let Some(notice_text) = notice_text {
        let notice = Paragraph::new(notice_text)
            .style(Style::default().fg(Color::Yellow))
            .scroll((state.notice_scroll, 0))
            .wrap(Wrap { trim: false });
        f.render_widget(notice, chunks[2]);
    }

    // Filter input
    let filter_block = Block::default()
        .borders(Borders::ALL)
        .title(" filter (start typing) ");
    let filter = Paragraph::new(state.filter.as_str())
        .block(filter_block)
        .wrap(Wrap { trim: false });
    f.render_widget(filter, chunks[3]);

    // Footer
    let footer_text = if state.confirming_delete.is_some() && notice_scrollable {
        "y delete | n/Esc keep | PgUp/PgDn details"
    } else if state.confirming_delete.is_some() {
        "y delete | n/Esc keep session"
    } else if notice_scrollable && state.delete_supported {
        "Up/Down navigate | Enter select | Delete remove | PgUp/PgDn notice | Esc cancel"
    } else if notice_scrollable {
        "Up/Down navigate | Enter select | PgUp/PgDn notice | Esc cancel"
    } else if state.delete_supported {
        "Up/Down navigate | Enter select | Delete remove | Esc cancel"
    } else {
        "Up/Down navigate | Enter select | Esc cancel"
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[4]);
}

fn session_picker_notice_text(state: &SessionPickerState) -> Option<String> {
    if let Some(session) = state.delete_confirmation_session() {
        let label = session.title.as_deref().unwrap_or(&session.session_id);
        Some(format!(
            "Delete session \"{label}\" ({}) in {}? Press y to delete, n to keep it.",
            session.session_id,
            session.cwd.display()
        ))
    } else {
        state.notice.clone()
    }
}

fn session_picker_notice_height(area: ratatui::layout::Rect, notice: Option<&str>) -> u16 {
    let Some(notice) = notice else {
        return 0;
    };
    let reserved = 1 + 3 + 1 + 3;
    let available = area.height.saturating_sub(reserved).max(1);
    let desired = wrapped_line_count(notice, area.width.max(1)).min(u16::MAX as usize) as u16;
    desired.clamp(1, available)
}

fn notice_needs_scroll(
    area: ratatui::layout::Rect,
    notice: Option<&str>,
    notice_height: u16,
) -> bool {
    notice
        .map(|text| wrapped_line_count(text, area.width.max(1)) > notice_height as usize)
        .unwrap_or(false)
}

fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = width.max(1) as usize;
    text.lines()
        .map(|line| {
            let display_width = UnicodeWidthStr::width(line);
            display_width.div_ceil(width).max(1)
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::{
        AuthMethod, AuthMethodAgent, AuthenticateResponse, DeleteSessionResponse,
        InitializeResponse, ListSessionsResponse, SessionCapabilities, SessionDeleteCapabilities,
        SessionId, SessionListCapabilities,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::split;

    fn sample_sessions() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                session_id: "sess-1".into(),
                cwd: PathBuf::from("/home/user/project-a"),
                title: Some("Refactor auth module".into()),
                updated_at: Some("2025-01-15T10:30:00Z".into()),
            },
            SessionEntry {
                session_id: "sess-2".into(),
                cwd: PathBuf::from("/home/user/project-b"),
                title: None,
                updated_at: Some("2025-01-14T08:00:00Z".into()),
            },
            SessionEntry {
                session_id: "sess-3".into(),
                cwd: PathBuf::from("/tmp/scratch"),
                title: Some("Quick experiment".into()),
                updated_at: None,
            },
        ]
    }

    #[test]
    fn picker_state_empty_filter_shows_all() {
        let state = SessionPickerState::new(sample_sessions(), false, None);
        assert_eq!(state.filtered.len(), 3);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_by_title() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "auth".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-1");
    }

    #[test]
    fn picker_state_filter_by_cwd() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "scratch".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-3");
    }

    #[test]
    fn picker_state_filter_by_session_id() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "sess-2".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-2");
    }

    #[test]
    fn picker_state_move_selection_wraps() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, 2);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_preserves_selection_on_recompute() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        // Select the second item.
        state.move_selection(1);
        assert_eq!(state.selected, 1);
        // Now type a character that still matches all items.
        state.filter = "s".into();
        state.recompute_filter();
        // sess-2 should still be selected (it matches "s").
        assert_eq!(
            state.sessions[state.filtered[state.selected]].session_id,
            "sess-2"
        );
    }

    #[test]
    fn picker_state_filter_no_match_clears_selection() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        state.filter = "zzzz_no_match".into();
        state.recompute_filter();
        assert!(state.filtered.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_delete_key_requires_advertised_capability() {
        let mut state = SessionPickerState::new(sample_sessions(), false, None);
        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert!(state.confirming_delete.is_none());
    }

    #[test]
    fn picker_delete_confirmation_returns_delete_request() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        assert!(
            handle_session_picker_event(
                &mut state,
                CtEvent::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Delete,
                    KeyModifiers::NONE,
                )),
            )
            .is_none()
        );
        assert_eq!(state.confirming_delete.as_deref(), Some("sess-1"));

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )),
        );

        match outcome {
            Some(ResumeOutcome::DeleteRequested(entry)) => assert_eq!(entry.session_id, "sess-1"),
            other => panic!("expected delete request, got {other:?}"),
        }
    }

    #[test]
    fn picker_delete_confirmation_can_be_cancelled() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        let _ = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert!(state.confirming_delete.is_none());
    }

    #[test]
    fn picker_delete_confirmation_blocks_selection_until_resolved() {
        let mut state = SessionPickerState::new(sample_sessions(), true, None);
        let _ = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            )),
        );

        let outcome = handle_session_picker_event(
            &mut state,
            CtEvent::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
        );

        assert!(outcome.is_none());
        assert_eq!(state.confirming_delete.as_deref(), Some("sess-1"));
    }

    #[test]
    fn picker_notice_height_grows_for_wrapped_errors() {
        let area = ratatui::layout::Rect::new(0, 0, 20, 12);
        let notice =
            "Delete failed for duplicate-title: authentication required with a long diagnostic";

        let height = session_picker_notice_height(area, Some(notice));

        assert!(height > 1);
        assert!(notice_needs_scroll(area, Some(notice), 1));
    }

    #[test]
    fn picker_delete_confirmation_text_includes_session_identity() {
        let state = SessionPickerState::new(sample_sessions(), true, None);
        let mut state = state;
        state.request_delete_confirmation();

        let text = session_picker_notice_text(&state).expect("confirmation text");

        assert!(text.contains("sess-1"));
        assert!(text.contains("/home/user/project-a"));
    }

    #[test]
    fn session_entry_json_serializes() {
        let entry = SessionEntry {
            session_id: "sess-abc".into(),
            cwd: PathBuf::from("/home/user/project"),
            title: Some("My session".into()),
            updated_at: None,
        };
        let json = SessionEntryJson::from(&entry);
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(serialized.contains("sess-abc"));
        assert!(serialized.contains("My session"));
        assert!(!serialized.contains("updated_at"));
    }

    #[test]
    fn session_listing_rejects_unsupported_protocol_version() {
        let err = validate_protocol_version(ProtocolVersion::V0).expect_err("unsupported");
        assert!(err.to_string().contains("unsupported ACP protocol version"));
        assert!(validate_protocol_version(ProtocolVersion::LATEST).is_ok());
    }

    #[test]
    fn session_listing_requires_list_capability() {
        let err = require_session_list(&AgentCapabilities::new()).expect_err("missing");
        assert!(err.to_string().contains("sessionCapabilities.list"));

        let supported = AgentCapabilities::new()
            .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
        assert!(require_session_list(&supported).is_ok());
    }

    #[test]
    fn session_deletion_requires_delete_capability() {
        let err = require_session_delete(&AgentCapabilities::new()).expect_err("missing");
        assert!(err.to_string().contains("sessionCapabilities.delete"));

        let supported = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new().delete(SessionDeleteCapabilities::new()),
        );
        assert!(require_session_delete(&supported).is_ok());
    }

    async fn run_mock_agent_list_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(AtomicBool::new(false));
        let authenticate_seen = authenticated.clone();
        let list_authenticated = authenticated.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: InitializeRequest, responder, _cx| {
                    let client_info = req.client_info.expect("clientInfo");
                    assert_eq!(client_info.name, env!("CARGO_PKG_NAME"));
                    assert_eq!(client_info.version, env!("CARGO_PKG_VERSION"));
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().list(SessionListCapabilities::new()),
                            ))
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: AuthenticateRequest, responder, _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    if list_authenticated.load(Ordering::SeqCst) {
                        responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                            SessionId::new("listed-session"),
                            PathBuf::from("/tmp"),
                        )]))
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_list_empty_cursor_then_second_page(
        stream: tokio::io::DuplexStream,
        seen_empty_cursor: Arc<AtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().list(SessionListCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ListSessionsRequest, responder, _cx| {
                    if req.cursor.is_none() {
                        responder.respond(
                            ListSessionsResponse::new(vec![SessionInfo::new(
                                SessionId::new("first-page"),
                                PathBuf::from("/tmp"),
                            )])
                            .next_cursor("".to_string()),
                        )
                    } else {
                        assert_eq!(req.cursor.as_deref(), Some(""));
                        seen_empty_cursor.store(true, Ordering::SeqCst);
                        responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                            SessionId::new("second-page"),
                            PathBuf::from("/tmp"),
                        )]))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_list_with_delete_capability(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                        SessionId::new("delete-capable-session"),
                        PathBuf::from("/tmp"),
                    )]))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_delete_session(
        stream: tokio::io::DuplexStream,
        delete_seen: Arc<AtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "delete-me");
                    delete_seen.store(true, Ordering::SeqCst);
                    responder.respond(DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_delete_auth_required_then_authenticates(
        stream: tokio::io::DuplexStream,
    ) {
        let authenticated = Arc::new(AtomicBool::new(false));
        let authenticate_seen = authenticated.clone();
        let delete_authenticated = authenticated.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(
                                AgentCapabilities::new().session_capabilities(
                                    SessionCapabilities::new()
                                        .list(SessionListCapabilities::new())
                                        .delete(SessionDeleteCapabilities::new()),
                                ),
                            )
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: AuthenticateRequest, responder, _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "delete-me");
                    if delete_authenticated.load(Ordering::SeqCst) {
                        responder.respond(DeleteSessionResponse::new())
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_authenticates_and_retries_list() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_list_auth_required_then_authenticates(
            agent_side,
        ));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should authenticate and retry");

        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].session_id, "listed-session");
        assert!(!listing.delete_supported);

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_treats_empty_cursor_as_opaque() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let seen_empty_cursor = Arc::new(AtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_list_empty_cursor_then_second_page(
            agent_side,
            seen_empty_cursor.clone(),
        ));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should request the empty cursor page");

        assert!(seen_empty_cursor.load(Ordering::SeqCst));
        assert_eq!(listing.sessions.len(), 2);
        assert_eq!(listing.sessions[0].session_id, "first-page");
        assert_eq!(listing.sessions[1].session_id, "second-page");

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_reports_delete_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_list_with_delete_capability(agent_side));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should include delete capability");

        assert!(listing.delete_supported);
        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].session_id, "delete-capable-session");

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_deletion_sends_delete_request() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let delete_seen = Arc::new(AtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_delete_session(
            agent_side,
            delete_seen.clone(),
        ));

        delete_session_via_transport(client_transport, "delete-me".to_string())
            .await
            .expect("session deletion should succeed");

        assert!(delete_seen.load(Ordering::SeqCst));
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_deletion_authenticates_and_retries_delete() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_delete_auth_required_then_authenticates(
            agent_side,
        ));

        delete_session_via_transport(client_transport, "delete-me".to_string())
            .await
            .expect("session deletion should authenticate and retry");

        agent_task.abort();
    }
}
