//! The documented HTTP API an orchestrating agent drives sessions with.
//!
//! The web viewer's own `/api/...` routes exist for the browser: they are
//! undocumented, cookie-only, and shaped around what a phone renders. These
//! `/api/v1/...` routes are the stable surface instead. They authenticate with
//! a bearer token from a file the same user can read, answer with a version
//! header so a client can tell which contract it reached, and — the point of
//! the whole module — let a caller block until one specific prompt finishes and
//! read a structured outcome for it.
//!
//! Everything that needs the daemon's live session actors or its SQLite store
//! reaches them through [`SubagentBackend`]. The daemon implements it in
//! `server_runtime::api`; the route tests implement it with a hand-written fake,
//! so the HTTP contract is tested without a running daemon.

mod events;

use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult};
use axum::extract::{Path, Query, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, HeaderValue,
};
use axum::http::{Request as HttpRequest, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use mj_core::state::{
    MaterializedExecutionState, MaterializedTurn, MaterializedTurnOutcome, TurnOutcomeKind,
};

use mj_core::relay::{CapacityRetry, is_capacity_stop_reason};

use mj_client::session::{BoxFuture, SessionHandle};

use super::{
    ActionOutcome, ApiError, COOKIE_NAME, ControllerAction, ControllerRequest, ServerState,
    ViewerLifecycleCategory, ViewerSession, ViewerSnapshot, constant_time_eq, cookie_value,
    create_quick_bundle, now_unix, require_session_record, session_cookie_valid, validate_action,
    validate_prompt_text,
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
pub use mj_core::subagent::MAX_WAIT_SECONDS as MAX_WAIT_SECS;

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
mod wait_policy;
pub use wait_policy::*;
mod routes;
pub use routes::*;
mod config;
pub(crate) use config::*;
mod sessions;
use sessions::*;
mod subagents;
pub(crate) use subagents::*;
mod turns;
pub use turns::*;
mod files;
pub use files::*;
mod wait;
use wait::*;

#[cfg(test)]
mod tests;
