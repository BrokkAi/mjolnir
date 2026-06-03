//! Simple remote-control server and local session registration.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::SessionUpdate;
use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rcgen::generate_simple_self_signed;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::config::SelectedAgent;
use crate::event::{UiCommand, UiEvent};

const REMOTE_CONTROL_ADDR: &str = "127.0.0.1:11921";
const REMOTE_CONTROL_UPSERT_URL: &str = "https://localhost:11921/api/sessions";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub start_time: String,
    pub last_update: String,
    pub total_messages: u64,
    pub project: String,
    pub agent: String,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionTracker {
    client: Option<reqwest::Client>,
    state: Arc<Mutex<TrackerState>>,
    heartbeat: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug)]
struct TrackerState {
    session_id: Option<String>,
    name: Option<String>,
    start_time: Option<String>,
    last_update: Option<String>,
    total_messages: u64,
    project: String,
    agent: String,
    agent_message_open: bool,
}

#[derive(Debug, Clone)]
struct ServerPaths {
    db_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ServerState {
    db_path: Arc<PathBuf>,
}

impl TrackerState {
    fn new(project: String, agent: String) -> Self {
        Self {
            session_id: None,
            name: None,
            start_time: None,
            last_update: None,
            total_messages: 0,
            project,
            agent,
            agent_message_open: false,
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        if matches!(command, UiCommand::SendPrompt { .. }) {
            self.total_messages = self.total_messages.saturating_add(1);
            self.agent_message_open = false;
            self.touch();
        }
    }

    fn observe_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::SessionStarted { session_id, .. } => {
                let now = now_rfc3339();
                let first_start = self.session_id.is_none();
                self.session_id = Some(session_id.clone());
                if self.name.is_none() {
                    self.name = Some(session_id.clone());
                }
                if self.start_time.is_none() {
                    self.start_time = Some(now.clone());
                }
                self.last_update = Some(now);
                self.agent_message_open = false;
                first_start
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update(update);
                false
            }
            UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => {
                self.agent_message_open = false;
                self.touch();
                false
            }
            UiEvent::Connected { .. }
            | UiEvent::SessionConfigOptions { .. }
            | UiEvent::PermissionRequest(_)
            | UiEvent::Warning(_) => false,
        }
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(_) => {
                if !self.agent_message_open {
                    self.total_messages = self.total_messages.saturating_add(1);
                    self.agent_message_open = true;
                }
                self.touch();
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.name = Some(title.clone());
                }
                self.agent_message_open = false;
                self.touch();
            }
            _ => {
                self.agent_message_open = false;
                self.touch();
            }
        }
    }

    fn snapshot(&self) -> Option<SessionRecord> {
        let session_id = self.session_id.clone()?;
        let start_time = self.start_time.clone()?;
        let last_update = self.last_update.clone()?;
        Some(SessionRecord {
            name: self.name.clone().unwrap_or_else(|| session_id.clone()),
            session_id,
            start_time,
            last_update,
            total_messages: self.total_messages,
            project: self.project.clone(),
            agent: self.agent.clone(),
        })
    }

    fn snapshot_with_heartbeat_touch(&mut self) -> Option<SessionRecord> {
        self.touch();
        self.snapshot()
    }

    fn touch(&mut self) {
        self.last_update = Some(now_rfc3339());
    }
}

impl RemoteSessionTracker {
    pub fn new(project: String, agent: String) -> Self {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .ok();
        Self {
            client,
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
            heartbeat: Arc::new(Mutex::new(None)),
        }
    }

    pub fn observe_command(&self, command: &UiCommand) {
        if let Ok(mut state) = self.state.lock() {
            state.observe_command(command);
        }
        self.spawn_flush();
    }

    pub fn observe_event(&self, event: &UiEvent) {
        let started = if let Ok(mut state) = self.state.lock() {
            state.observe_event(event)
        } else {
            false
        };
        if started {
            self.ensure_heartbeat();
        }
        self.spawn_flush();
    }

    pub async fn shutdown(&self) {
        let handle = self.heartbeat.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let snapshot = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.snapshot_with_heartbeat_touch());
        if let Some(snapshot) = snapshot
            && let Err(error) = send_snapshot(client, snapshot).await
        {
            debug!("final remote-control flush failed: {error:#}");
        }
    }

    fn ensure_heartbeat(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Ok(mut slot) = self.heartbeat.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let state = Arc::clone(&self.state);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let snapshot = state
                    .lock()
                    .ok()
                    .and_then(|mut state| state.snapshot_with_heartbeat_touch());
                let Some(snapshot) = snapshot else {
                    continue;
                };
                if let Err(error) = send_snapshot(client.clone(), snapshot).await {
                    debug!("remote-control heartbeat failed: {error:#}");
                }
            }
        }));
    }

    fn spawn_flush(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let snapshot = self.state.lock().ok().and_then(|state| state.snapshot());
        let Some(snapshot) = snapshot else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = send_snapshot(client, snapshot).await {
                debug!("remote-control flush failed: {error:#}");
            }
        });
    }
}

pub async fn run_server() -> Result<()> {
    let paths = ensure_server_paths()?;
    init_db(&paths.db_path)?;

    let app = Router::new()
        .route("/sessions", get(list_sessions))
        .route("/api/sessions", post(upsert_session))
        .with_state(ServerState {
            db_path: Arc::new(paths.db_path.clone()),
        });

    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert_path, &paths.key_path)
            .await
            .context("load remote-control TLS certificate")?;

    println!("Remote control listening on https://localhost:11921");
    axum_server::bind_rustls(REMOTE_CONTROL_ADDR.parse()?, tls_config)
        .serve(app.into_make_service())
        .await
        .context("serve remote-control API")
}

pub fn agent_display_label(agent: &SelectedAgent) -> String {
    if agent.source_id == "custom" {
        let mut words = Vec::with_capacity(agent.args.len() + 1);
        words.push(agent.program.to_string_lossy().into_owned());
        words.extend(agent.args.iter().cloned());
        shell_words::join(words)
    } else {
        agent.source_id.clone()
    }
}

pub fn project_label_from_cwd(cwd: &Path) -> String {
    if let Some(parent) = parent_above_mjolnir(cwd) {
        return folder_label(&parent);
    }
    folder_label(cwd)
}

async fn upsert_session(
    State(state): State<ServerState>,
    Json(session): Json<SessionRecord>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        upsert_session_record(db_path.as_ref().as_path(), &session)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn list_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let sessions =
        tokio::task::spawn_blocking(move || load_session_records(db_path.as_ref().as_path()))
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;
    Ok(Json(sessions))
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn ensure_server_paths() -> Result<ServerPaths> {
    let root = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote-control");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;

    let cert_path = root.join("cert.pem");
    let key_path = root.join("key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let cert = generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ])
        .context("generate localhost self-signed certificate")?;
        std::fs::write(&cert_path, cert.cert.pem())
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())
            .with_context(|| format!("write {}", key_path.display()))?;
    }

    Ok(ServerPaths {
        db_path: root.join("sessions.sqlite3"),
        cert_path,
        key_path,
    })
}

fn init_db(db_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    conn.execute_batch(
        "create table if not exists sessions (
            session_id text primary key,
            name text not null,
            start_time text not null,
            last_update text not null,
            total_messages integer not null,
            project text not null,
            agent text not null
        );",
    )
    .context("create remote-control schema")?;
    Ok(())
}

fn open_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("set sqlite journal mode")?;
    Ok(conn)
}

fn upsert_session_record(db_path: &Path, session: &SessionRecord) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let total_messages =
        i64::try_from(session.total_messages).context("total_messages exceeds sqlite integer")?;
    conn.execute(
        "insert into sessions (
            session_id,
            name,
            start_time,
            last_update,
            total_messages,
            project,
            agent
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        on conflict(session_id) do update set
            name = excluded.name,
            start_time = sessions.start_time,
            last_update = excluded.last_update,
            total_messages = excluded.total_messages,
            project = excluded.project,
            agent = excluded.agent",
        params![
            session.session_id,
            session.name,
            session.start_time,
            session.last_update,
            total_messages,
            session.project,
            session.agent,
        ],
    )
    .context("upsert remote-control session")?;
    Ok(())
}

fn load_session_records(db_path: &Path) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                total_messages,
                project,
                agent
            from sessions
            order by last_update desc, session_id asc",
        )
        .context("prepare session query")?;
    let rows = stmt
        .query_map([], |row| {
            let total_messages: i64 = row.get(4)?;
            Ok(SessionRecord {
                session_id: row.get(0)?,
                name: row.get(1)?,
                start_time: row.get(2)?,
                last_update: row.get(3)?,
                total_messages: u64::try_from(total_messages).unwrap_or(0),
                project: row.get(5)?,
                agent: row.get(6)?,
            })
        })
        .context("query sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")
}

async fn send_snapshot(client: reqwest::Client, snapshot: SessionRecord) -> Result<()> {
    client
        .post(REMOTE_CONTROL_UPSERT_URL)
        .json(&snapshot)
        .send()
        .await
        .context("send remote-control update")?
        .error_for_status()
        .context("remote-control server returned an error")?;
    Ok(())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn folder_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn parent_above_mjolnir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".mjolnir"))
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_counts_user_prompts_and_agent_replies() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hi"),
                ),
            ),
        ));
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" again"),
                ),
            ),
        ));

        assert_eq!(state.total_messages, 2);
    }

    #[test]
    fn sqlite_upsert_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-03T10:00:00Z".to_string(),
            last_update: "2026-06-03T10:00:20Z".to_string(),
            total_messages: 4,
            project: "mjolnir".to_string(),
            agent: "anvil".to_string(),
        };

        upsert_session_record(&db_path, &session).expect("insert");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                total_messages: 6,
                last_update: "2026-06-03T10:00:40Z".to_string(),
                ..session.clone()
            },
        )
        .expect("update");

        let sessions = load_session_records(&db_path).expect("load");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "demo");
        assert_eq!(sessions[0].total_messages, 6);
        assert_eq!(sessions[0].start_time, "2026-06-03T10:00:00Z");
        assert_eq!(sessions[0].last_update, "2026-06-03T10:00:40Z");
    }
}
