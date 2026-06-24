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
    AgentCapabilities, AuthMethod, AuthenticateRequest, ErrorCode, InitializeRequest,
    ListSessionsRequest, ProtocolVersion, SessionInfo,
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

/// Outcome of the interactive session picker.
#[derive(Debug)]
pub enum ResumeOutcome {
    /// User selected a session to resume.
    Selected(SessionEntry),
    /// User cancelled with Esc.
    Cancelled,
}

/// List sessions from the configured agent without entering the TUI.
pub async fn list_sessions(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
) -> Result<Vec<SessionEntry>> {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let prepared = acp::prepare_agent_command_for_spawn(&agent.program, &agent.env, &ui_tx)
        .await
        .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
        .context("prepare agent for session listing")?;

    let (mut child, child_stdin, child_stdout) =
        acp::spawn_agent(&prepared.command, &agent.args, &prepared.env, agent_stderr)
            .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
            .context("spawn agent for session listing")?;

    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    let sessions = list_sessions_via_transport(transport, cwd).await;

    // Clean up: kill the agent process and wait for it to exit.
    if let Err(e) = child.kill().await {
        tracing::warn!("kill agent after listing: {e}");
    }
    let _ = child.wait().await;

    sessions
}

/// Drive the ACP client to list sessions over an existing transport.
async fn list_sessions_via_transport<T>(transport: T, cwd: PathBuf) -> Result<Vec<SessionEntry>>
where
    T: ConnectTo<Client>,
{
    let result = Client
        .builder()
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            // Initialize handshake.
            let init_req = InitializeRequest::new(ProtocolVersion::V1);
            let init_resp = conn
                .send_request(init_req)
                .block_task()
                .await
                .context("initialize for session listing")?;
            validate_protocol_version(init_resp.protocol_version)?;
            require_session_list(&init_resp.agent_capabilities)?;

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
                    Some(next) if !next.is_empty() => cursor = Some(next),
                    _ => break,
                }
            }

            Ok(all_sessions)
        })
        .await;

    result.context("ACP client error during session listing")
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

/// Interactive session picker state.
struct SessionPickerState {
    sessions: Vec<SessionEntry>,
    filter: String,
    filtered: Vec<usize>,
    selected: usize,
}

impl SessionPickerState {
    fn new(sessions: Vec<SessionEntry>) -> Self {
        let mut state = Self {
            sessions,
            filter: String::new(),
            filtered: Vec::new(),
            selected: 0,
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
}

/// Run the interactive session picker until the user selects or cancels.
pub async fn run_session_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    sessions: Vec<SessionEntry>,
) -> Result<ResumeOutcome> {
    let mut state = SessionPickerState::new(sessions);

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
        (_, KeyCode::Enter) => state
            .focused_session()
            .cloned()
            .map(ResumeOutcome::Selected),
        (_, KeyCode::Backspace) => {
            state.filter.pop();
            state.recompute_filter();
            None
        }
        (_, KeyCode::Char(c)) => {
            state.filter.push(c);
            state.recompute_filter();
            None
        }
        _ => None,
    }
}

fn draw_session_picker(f: &mut ratatui::Frame, state: &SessionPickerState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
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

    // Filter input
    let filter_block = Block::default()
        .borders(Borders::ALL)
        .title(" filter (start typing) ");
    let filter = Paragraph::new(state.filter.as_str())
        .block(filter_block)
        .wrap(Wrap { trim: false });
    f.render_widget(filter, chunks[2]);

    // Footer
    let footer = Paragraph::new("Up/Down navigate | Enter select | Esc cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::{
        AuthMethod, AuthMethodAgent, AuthenticateResponse, InitializeResponse,
        ListSessionsResponse, SessionCapabilities, SessionId, SessionListCapabilities,
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
        let state = SessionPickerState::new(sample_sessions());
        assert_eq!(state.filtered.len(), 3);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_by_title() {
        let mut state = SessionPickerState::new(sample_sessions());
        state.filter = "auth".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-1");
    }

    #[test]
    fn picker_state_filter_by_cwd() {
        let mut state = SessionPickerState::new(sample_sessions());
        state.filter = "scratch".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-3");
    }

    #[test]
    fn picker_state_filter_by_session_id() {
        let mut state = SessionPickerState::new(sample_sessions());
        state.filter = "sess-2".into();
        state.recompute_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.sessions[state.filtered[0]].session_id, "sess-2");
    }

    #[test]
    fn picker_state_move_selection_wraps() {
        let mut state = SessionPickerState::new(sample_sessions());
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, 2);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn picker_state_filter_preserves_selection_on_recompute() {
        let mut state = SessionPickerState::new(sample_sessions());
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
        let mut state = SessionPickerState::new(sample_sessions());
        state.filter = "zzzz_no_match".into();
        state.recompute_filter();
        assert!(state.filtered.is_empty());
        assert_eq!(state.selected, 0);
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

    async fn run_mock_agent_list_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(AtomicBool::new(false));
        let authenticate_seen = authenticated.clone();
        let list_authenticated = authenticated.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_authenticates_and_retries_list() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_list_auth_required_then_authenticates(
            agent_side,
        ));

        let sessions = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should authenticate and retry");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "listed-session");

        agent_task.abort();
    }
}
