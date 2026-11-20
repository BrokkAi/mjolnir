//! Project image build operations, shared by the HTTP API and its clients.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRequest {
    pub path: PathBuf,
    pub bundle: Option<String>,
    pub target: Option<String>,
    pub name: Option<String>,
    pub session_id: Option<String>,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub print: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageStarted {
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageFollow {
    pub log: String,
    pub next_offset: usize,
    pub result: Option<Result<ImageResult, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageResult {
    pub containerfile: Option<String>,
    pub image: Option<String>,
    pub skipped: bool,
    pub status: Vec<ImageStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageStatus {
    pub platform: String,
    pub image: String,
    /// None when freshness could not be determined.
    pub current: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_builder: bool,
}
