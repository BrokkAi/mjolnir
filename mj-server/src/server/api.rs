//! The documented HTTP API an orchestrating agent drives sessions with.
//!
//! The web viewer and other API clients share the `/api/v1` router. Clients
//! authenticate with a bearer token from an owner-readable file or a signed
//! viewer cookie. Settings routes require the viewer cookie. Responses carry
//! a version header so clients can identify the contract they reached.
//!
//! Session operations reach live actors and SQLite through [`SubagentBackend`]
//! and controller request channels implemented by the daemon runtime.
//!
//! # The artifact routes are contract
//!
//! A caller that runs work somewhere other than this machine cannot read the
//! working tree to decide whether a change is safe to publish, so four routes
//! are part of the stable contract rather than conveniences. Changing any of
//! their media types, response bodies, or status meanings is a change to this
//! API:
//!
//! - `GET /sessions/{session_id}/diff` answers a diffstat or a unified patch
//!   for selected paths as `text/x-diff; charset=utf-8`, comparing from the revision named
//!   in `base` when it names one. With `json=true` it answers
//!   `application/json` carrying `mj_checkpoint::archive::SessionDiff` instead,
//!   a type that lives in the checkpoint crate, so changing its fields is also
//!   a change here.
//! - `POST /sessions/{session_id}/export` answers the work in the form the
//!   caller asks for: `kind: "patch"` as `text/x-diff`, `kind: "branch"` as
//!   `{"branch", "remote"}`, and `kind: "bundle"` as
//!   `application/octet-stream` with the bundle named in an attachment
//!   filename.
//! - `GET` and `PUT /sessions/{session_id}/files` read one file as
//!   `application/octet-stream` and inject one, answering `{"path", "bytes"}`.
//!   Injection requires a live, idle session, because a write into a running
//!   turn has no meaning.
//! - `GET /sessions/{session_id}/transcript` answers a page of the transcript,
//!   with `next_after_seq` as the cursor to continue from and `latest_seq` to
//!   tell whether the page reached the end.
//!
//! Where a route can refuse because of the session's own state — an export with
//! nothing to export, an injection into a session that is neither live nor idle
//! — it answers 409 with a sentence the caller can act on. A failure that is not
//! the caller's to fix is a 5xx. Keeping those apart is part of the contract,
//! because one is a decision for a person and the other is not.

mod events;

use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult};
use axum::extract::{Path, Query, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderValue,
};
use axum::http::{Request as HttpRequest, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use mj_core::state::{MaterializedExecutionState, MaterializedTurn, MaterializedTurnOutcome};

use mj_core::relay::CapacityRetry;

use mj_client::session::{BoxFuture, SessionHandle};

use super::{
    ActionOutcome, AdditionalMount, ApiError, BackgroundTaskStopFailure, BundleFailure,
    CompletionHost, CompletionKind, ControllerAction, ControllerRequest, CreateBundleResponse,
    MAX_PROMPT_TEXT_BODY_BYTES, MovePreparation, MoveSelection, PathCompletion, PreflightFailure,
    PreflightNew, PreflightResume, Refusal, ServerState, SessionResourceAllocation,
    ViewerClientState, ViewerPromptHistory, ViewerSession, ViewerSnapshot, constant_time_eq,
    named_body_limit, require_session_record, validate_action, validate_prompt_text,
};

/// Response header naming the contract version this server speaks. A client
/// that understands only version 1 can refuse anything else without parsing a
/// body it may not recognize.
pub const API_VERSION_HEADER: &str = "mj-api-version";
pub const API_VERSION: &str = "1";

/// How long a wait blocks when the caller names no timeout, and the ceiling it
/// may ask for. Both are generous: a turn routinely runs for minutes, and the
/// caller is a program that reconnects rather than a person holding a page.
pub const DEFAULT_WAIT_SECS: u64 = 600;
pub const MAX_WAIT_SECS: u64 = 3_600;

/// How often a wait re-reads durable state for a session with no live actor.
const STOPPED_POLL_INTERVAL: Duration = Duration::from_millis(500);

const API_TOKEN_FILE: &str = "api-token";
const API_TOKEN_BYTES: usize = 32;

mod token;
pub use token::*;
mod failure;
pub use failure::*;
mod types;
pub use types::*;
mod subagent_backend;
pub use subagent_backend::*;
mod github;
use github::github_token;
mod mailbox;
pub use mj_session_runtime::wait_policy::*;
mod routes;
pub(super) use routes::loopback_authenticated;
pub use routes::*;
mod config;
pub use config::*;
mod options;
use options::{options, saved_default};
mod sessions;
use sessions::*;
mod ui;
use ui::*;
mod subagents;
pub use subagents::*;
mod turns;
pub use turns::*;
mod checkpoints;
pub use checkpoints::*;
mod files;
pub use files::*;
mod wait;
use wait::*;
mod workspaces;
use workspaces::*;

#[cfg(test)]
mod tests;

mod images;
