use super::*;
use crate::daemon::login_terminal::{TerminalInput, TerminalOutput};
use crate::daemon::{RecoveryAdoptionProgress, RecoveryAdoptionRequest};
use crate::runtime_feed::{RuntimeCursor, RuntimeFrame};
use std::collections::BTreeSet;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

pub const LOGIN_TERMINAL_ID_HEADER: &str = "x-mjolnir-terminal-id";

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeChangesRequest {
    pub cursor: Option<RuntimeCursor>,
    pub wait: bool,
    pub subagent_parents: BTreeSet<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginTerminalRequest {
    pub profile_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum AdoptionEvent {
    Waiting {
        blockers: Vec<String>,
        destroy_command: Option<String>,
    },
    Installed {
        session_id: String,
    },
    Done,
    Failed {
        error: String,
    },
}

type ResponseStream = futures::stream::BoxStream<'static, std::io::Result<Bytes>>;

struct Ndjson {
    reader: BufReader<StreamReader<ResponseStream, Bytes>>,
    line: Vec<u8>,
}

impl Ndjson {
    fn new(response: Response) -> Self {
        use futures::TryStreamExt;
        let stream = response.bytes_stream().map_err(std::io::Error::other);
        Self {
            reader: BufReader::new(StreamReader::new(Box::pin(stream))),
            line: Vec::new(),
        }
    }

    async fn next<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.reader
            .read_until(b'\n', &mut self.line)
            .await
            .context(DaemonTransportLost)?;
        ensure!(self.line.last() == Some(&b'\n'), DaemonTransportLost);
        let event = serde_json::from_slice(&self.line);
        self.line.clear();
        Ok(event?)
    }
}

pub struct HttpLoginTerminal {
    output: Ndjson,
    input: HttpLoginTerminalInput,
}

#[derive(Clone)]
pub struct HttpLoginTerminalInput {
    client: reqwest::Client,
    endpoint: EndpointState,
    path: String,
}

impl HttpLoginTerminal {
    pub async fn next_output(&mut self) -> Result<TerminalOutput> {
        self.output.next().await
    }

    pub fn input(&self) -> HttpLoginTerminalInput {
        self.input.clone()
    }

    pub async fn send_input(&self, input: TerminalInput) -> Result<()> {
        self.input.send_input(input).await
    }
}

impl HttpLoginTerminalInput {
    pub async fn send_input(&self, input: TerminalInput) -> Result<()> {
        let mut request = self
            .client
            .post(self.endpoint.url(&self.path))
            .json(&input)
            .build()?;
        self.endpoint.authorize(&mut request)?;
        HttpDaemonClient::send_once(&self.client, request).await?;
        Ok(())
    }
}

impl HttpDaemonClient {
    pub async fn runtime_changes(
        &self,
        cursor: Option<RuntimeCursor>,
        wait: bool,
    ) -> Result<RuntimeFrame> {
        self.runtime_changes_for_subagent_parents(cursor, wait, BTreeSet::new())
            .await
    }

    pub async fn runtime_changes_for_subagent_parents(
        &self,
        cursor: Option<RuntimeCursor>,
        wait: bool,
        subagent_parents: BTreeSet<String>,
    ) -> Result<RuntimeFrame> {
        let body = RuntimeChangesRequest {
            cursor,
            wait,
            subagent_parents,
        };
        let (response, _lease) = self
            .send(
                Method::POST,
                "/api/v1/runtime/changes",
                |request| request.json(&body),
                false,
            )
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<HttpApiError>()
                    .is_some_and(|error| error.status == StatusCode::SERVICE_UNAVAILABLE)
                    || error
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(|error| error.is_connect() || error.is_request())
                {
                    error.context(DaemonTransportLost)
                } else {
                    error
                }
            })?;
        // Reqwest's JSON helper reports an interrupted body as a decode error.
        // Read it first so the caller can distinguish a severed connection.
        let body = response.bytes().await.context(DaemonTransportLost)?;
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn adopt_recovery(
        &self,
        adoption: RecoveryAdoptionRequest,
        mut progress: impl FnMut(RecoveryAdoptionProgress),
    ) -> Result<()> {
        let (response, _lease) = self
            .send(
                Method::POST,
                "/api/v1/recovery/adopt",
                |request| request.json(&adoption),
                false,
            )
            .await?;
        let mut events = Ndjson::new(response);
        loop {
            match events.next().await.context("adoption connection ended before completion; check mj sessions, then rerun recover adopt if needed")? {
                AdoptionEvent::Waiting { blockers, destroy_command } =>
                    progress(RecoveryAdoptionProgress::Waiting { blockers, destroy_command }),
                AdoptionEvent::Installed { session_id } =>
                    progress(RecoveryAdoptionProgress::Installed { session_id }),
                AdoptionEvent::Done => return Ok(()),
                AdoptionEvent::Failed { error } => anyhow::bail!("{error}"),
            }
        }
    }

    pub async fn open_login_terminal(
        &self,
        profile_id: String,
        cols: u16,
        rows: u16,
    ) -> Result<HttpLoginTerminal> {
        let endpoint = self.endpoint.read().await.clone();
        let mut request = self
            .http
            .post(endpoint.url("/api/v1/login-terminal"))
            .json(&LoginTerminalRequest {
                profile_id,
                cols,
                rows,
            })
            .build()?;
        endpoint.authorize(&mut request)?;
        let response = Self::send_once(&self.http, request).await?;
        let id = response
            .headers()
            .get(LOGIN_TERMINAL_ID_HEADER)
            .context("daemon did not provide a login terminal ID")?
            .to_str()?;
        let path = format!("/api/v1/login-terminal/{}/input", encode_segment(id)?);
        Ok(HttpLoginTerminal {
            output: Ndjson::new(response),
            input: HttpLoginTerminalInput {
                client: self.http.clone(),
                endpoint,
                path,
            },
        })
    }
}
