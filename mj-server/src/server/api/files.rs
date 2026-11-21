use super::*;

/// A unified diff of everything the session changed.
pub(super) async fn diff(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(options): Query<DiffOptions>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let json = options.json;
    let diff = backend.diff(session_id, options).await?;
    if json {
        let details: mj_checkpoint::archive::SessionDiff =
            serde_json::from_str(&diff).context("decode session diff metadata")?;
        return Ok(Json(details).into_response());
    }
    Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
}

/// Reject a path a file injection can never resolve, before it costs a round
/// trip to the target.
///
/// An absolute path is always a mistake worth naming here. A `..` is not: it is
/// how a multi-repo bundle names a sibling repository, now that a path resolves
/// in the directory the agent runs in rather than at the workspace root
/// (#1079). How far `..` may climb depends on the session's layout, which only
/// the daemon holds, so that is refused there and answers 409.
fn validate_session_file_path(path: &std::path::Path) -> Result<(), ApiFailure> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(ApiFailure::bad_request(
            "path must be relative to the directory the session's agent runs in",
        ));
    }
    Ok(())
}

/// One file from the session's workspace, as bytes.
///
/// The path is checked here as well as on the target: a caller that spells an
/// absolute or escaping path has made a mistake worth naming, and there is no
/// reason to spend a round trip to the target discovering it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileQuery {
    pub path: PathBuf,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub path: PathBuf,
    pub bytes: u64,
}

const UPLOAD_CHANNEL_CAPACITY: usize = 8;

struct UploadQueueState {
    chunks: std::collections::VecDeque<axum::body::Bytes>,
    eof: bool,
    error: Option<String>,
    producer_dropped: bool,
    aborted: bool,
}

struct UploadChannel {
    state: std::sync::Mutex<UploadQueueState>,
    readable: std::sync::Condvar,
    /// Bounds unread chunks to [`UPLOAD_CHANNEL_CAPACITY`] so a fast HTTP
    /// client cannot outrun a slow target pipe.
    space: tokio::sync::Semaphore,
}

impl UploadChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(UploadQueueState {
                chunks: std::collections::VecDeque::with_capacity(UPLOAD_CHANNEL_CAPACITY),
                eof: false,
                error: None,
                producer_dropped: false,
                aborted: false,
            }),
            readable: std::sync::Condvar::new(),
            space: tokio::sync::Semaphore::new(UPLOAD_CHANNEL_CAPACITY),
        })
    }

    fn abort(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.aborted = true;
        state.chunks.clear();
        self.readable.notify_all();
        drop(state);
        self.space.close();
    }
}

struct UploadProducer {
    channel: Arc<UploadChannel>,
    completed: bool,
}

impl UploadProducer {
    fn new(channel: Arc<UploadChannel>) -> Self {
        Self {
            channel,
            completed: false,
        }
    }

    async fn push(&self, chunk: axum::body::Bytes) -> bool {
        let Ok(permit) = self.channel.space.acquire().await else {
            return false;
        };
        permit.forget();
        let mut state = self
            .channel
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.aborted {
            return false;
        }
        state.chunks.push_back(chunk);
        self.channel.readable.notify_one();
        true
    }

    fn finish_eof(mut self) {
        self.completed = true;
        let mut state = self
            .channel
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.eof = true;
        self.channel.readable.notify_all();
    }

    fn finish_error(mut self, message: String) {
        self.completed = true;
        let mut state = self
            .channel
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.error = Some(message);
        self.channel.readable.notify_all();
    }
}

impl Drop for UploadProducer {
    fn drop(&mut self) {
        if !self.completed {
            let mut state = self
                .channel
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.producer_dropped = true;
            self.channel.readable.notify_all();
        }
    }
}

struct UploadChannelReader {
    channel: Arc<UploadChannel>,
    current: axum::body::Bytes,
    finished: bool,
}

impl UploadChannelReader {
    fn new(channel: Arc<UploadChannel>) -> Self {
        Self {
            channel,
            current: axum::body::Bytes::new(),
            finished: false,
        }
    }
}

impl Drop for UploadChannelReader {
    fn drop(&mut self) {
        self.channel.abort();
    }
}

impl std::io::Read for UploadChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio_util::bytes::Buf as _;
        if buf.is_empty() || self.finished {
            return Ok(0);
        }
        while self.current.is_empty() {
            let mut state = self
                .channel
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if state.aborted {
                    self.finished = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "upload cancelled",
                    ));
                }
                if let Some(message) = state.error.clone() {
                    self.finished = true;
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, message));
                }
                if state.producer_dropped {
                    self.finished = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "upload stream closed before EOF",
                    ));
                }
                if let Some(chunk) = state.chunks.pop_front() {
                    drop(state);
                    self.channel.space.add_permits(1);
                    if !chunk.is_empty() {
                        self.current = chunk;
                        break;
                    }
                    state = self
                        .channel
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    continue;
                }
                if state.eof {
                    self.finished = true;
                    return Ok(0);
                }
                state = self
                    .channel
                    .readable
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        let count = buf.len().min(self.current.len());
        buf[..count].copy_from_slice(&self.current[..count]);
        self.current.advance(count);
        Ok(count)
    }
}

pub(super) async fn write_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<WriteFileQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Result<Json<WriteFileResponse>, ApiFailure> {
    use futures::StreamExt as _;
    validate_session_file_path(&query.path)?;
    let expected_len = match headers.get(axum::http::header::CONTENT_LENGTH) {
        Some(value) => {
            let text = value
                .to_str()
                .map_err(|_| ApiFailure::bad_request("invalid Content-Length header"))?;
            let len = text
                .trim()
                .parse::<u64>()
                .map_err(|_| ApiFailure::bad_request("invalid Content-Length header"))?;
            Some(len)
        }
        None => None,
    };
    {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.is_idle || session.lifecycle != ViewerLifecycleCategory::Live {
            return Err(ApiFailure::conflict(
                "session must be live and idle for file injection",
            ));
        }
    }
    let backend = backend(&state)?;
    let channel = UploadChannel::new();
    let producer = UploadProducer::new(channel.clone());
    let abort_channel = channel.clone();
    let upload = UploadBody::new(
        Box::new(UploadChannelReader::new(channel)),
        Arc::new(move || abort_channel.abort()),
    );
    let pump = async move {
        let mut stream = body.into_data_stream();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) if chunk.is_empty() => continue,
                Ok(chunk) => {
                    if !producer.push(chunk).await {
                        return;
                    }
                }
                Err(error) => {
                    producer.finish_error(format!("read upload body: {error}"));
                    return;
                }
            }
        }
        producer.finish_eof();
    };
    let mut write_fut = std::pin::pin!(backend.write_file(
        session_id,
        query.path.clone(),
        upload,
        expected_len,
        query.overwrite,
    ));
    let written = tokio::select! {
        result = &mut write_fut => result?,
        () = pump => write_fut.await?,
    };
    Ok(Json(WriteFileResponse {
        path: query.path,
        bytes: written,
    }))
}

pub(super) async fn elicitations(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<mj_core::elicitation::ElicitationRequest>>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(super::types::v1_elicitations(
        &require_session_record(&snapshot, &session_id)?.pending_elicitations,
    )))
}

pub(super) async fn respond_elicitation(
    State(state): State<ServerState>,
    Path((session_id, elicitation_id)): Path<(String, String)>,
    Json(response): Json<mj_core::elicitation::ElicitationResponse>,
) -> Result<StatusCode, ApiFailure> {
    send_action(
        &state,
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        },
    )
    .await
}

/// The response header naming where a fetched path resolved on the target. It
/// is form-urlencoded, because a path need not be valid header text.
pub const FETCHED_PATH_HEADER: &str = "x-mj-fetched-path";

/// The target path a [`FETCHED_PATH_HEADER`] value names.
///
/// The value is one form-urlencoded token with no `=` or `&` left in it, so it
/// parses as a single key.
pub fn decode_fetched_path(value: &[u8]) -> Option<String> {
    url::form_urlencoded::parse(value)
        .next()
        .map(|(path, _)| path.into_owned())
        .filter(|path| !path.is_empty())
}

/// A file as bytes, a directory as a zip archive, or with `list=true` a JSON
/// listing of either, from the session's target.
///
/// A relative path resolves in the directory the agent runs in. An absolute
/// one is taken as the target spells it. The target accepts either only inside
/// the session's workspace, its sub-agent report root or its temporary
/// directory, after resolving symlinks.
pub(super) async fn read_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let path = PathBuf::from(query.path.trim());
    if path.as_os_str().is_empty() {
        return Err(ApiFailure::bad_request(
            "path must name a file or directory on the session's target",
        ));
    }
    let mode = if query.list {
        mj_checkpoint::archive::FetchMode::Listing
    } else {
        mj_checkpoint::archive::FetchMode::Content
    };
    let fetched = tokio::select! {
        result = backend.read_file(session_id, path, mode) => result?,
        // A manual restart also interrupts a repeatable read. Tell the client
        // it may reconnect even when this is not an automatic upgrade.
        () = state.shutdown.cancelled() => return Err(ApiFailure::unavailable(
            "the daemon stopped during fetch; reconnect and retry the download",
        ).with_code(Some(DAEMON_HANDOFF_CODE))),
    };
    let content_type = match fetched.kind {
        mj_checkpoint::archive::FetchedKind::File => "application/octet-stream",
        mj_checkpoint::archive::FetchedKind::Directory => "application/zip",
        mj_checkpoint::archive::FetchedKind::Listing => "application/json",
    };
    let resolved: String = url::form_urlencoded::byte_serialize(fetched.path.as_bytes()).collect();
    let length = fetched.len();
    let shutdown = state.shutdown.clone();
    let stream = futures::stream::try_unfold(fetched.reader(), move |mut reader| {
        let shutdown = shutdown.clone();
        async move {
            tokio::select! {
                biased;
                // EOF means the whole artifact was delivered. An interrupted
                // stream must fail even if compression removes Content-Length.
                () = shutdown.cancelled() => Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "daemon stopped during fetched payload delivery",
                )),
                result = async move {
                    mj_core::runtime::spawn_blocking(move || {
                        // An I/O chunk, not a transfer size limit.
                        let mut chunk = vec![0; 64 * 1024];
                        let count = std::io::Read::read(&mut reader, &mut chunk)?;
                        chunk.truncate(count);
                        Ok::<_, std::io::Error>(if count == 0 { None } else { Some((chunk, reader)) })
                    }).await.map_err(std::io::Error::other)?
                } => result,
            }
        }
    });
    Ok((
        [
            (CONTENT_TYPE, content_type.to_owned()),
            (axum::http::header::CONTENT_LENGTH, length.to_string()),
            (
                axum::http::HeaderName::from_static(FETCHED_PATH_HEADER),
                resolved,
            ),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

/// Get the session's work out, in whichever form the caller asked for.
pub(super) async fn export(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<ExportRequest>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    match request.kind {
        ExportKind::Patch => {
            let diff = backend.diff(session_id, DiffOptions::default()).await?;
            Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
        }
        ExportKind::Branch => {
            let branch = request
                .branch
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .ok_or_else(|| ApiFailure::bad_request("a branch export needs a branch name"))?
                .to_owned();
            let pushed = backend.push_branch(session_id, branch).await?;
            Ok(Json(pushed).into_response())
        }
        ExportKind::Bundle => {
            let bundle = backend.bundle(session_id.clone()).await?;
            // The filename reaches a header, so keep it to characters that
            // cannot end the quoted string or split the response.
            let filename: String = format!("{session_id}-{}.bundle", bundle.repository)
                .chars()
                .map(|character| match character {
                    'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' => character,
                    _ => '-',
                })
                .collect();
            Ok((
                [
                    (CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (
                        CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bundle.bytes,
            )
                .into_response())
        }
    }
}
