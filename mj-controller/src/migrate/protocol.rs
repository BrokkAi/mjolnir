//! The stream protocol between `mj migrate` (source) and `mj migrate-receive`
//! (destination).
//!
//! A frame is a `u32` big-endian length followed by that many bytes: a `u32`
//! big-endian JSON header length, the JSON header, then a raw payload. The
//! payload carries compressed file or block bytes, so file data is never
//! base64-encoded. A frame never exceeds [`MAX_FRAME_BYTES`].
//!
//! Only three requests move data: `PutFile` (start, chunks, end), `PatchBlocks`
//! (begin, blocks, end) and `RemoveFile`. Everything else is control.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_store::database::{MigrationRewriteSpec, StoreFenceRecord, StoreRewriteSummary};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::rewrite::StagedConfigRewriteSpec;

pub const PROTOCOL_VERSION: u32 = 1;
/// The largest frame either side accepts, and the bound on what it allocates for one.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Database block and file chunk size. A compressed block never exceeds
/// `MAX_FRAME_BYTES`: zstd's worst case is the input plus a small fraction.
pub const BLOCK_BYTES: usize = 1024 * 1024;
pub const ZSTD_LEVEL: i32 = 1;
/// The sender reads acknowledgements once this many are outstanding. An
/// acknowledgement frame is about thirty bytes, so a full window stays far
/// below the smallest pipe buffer (4 KiB) and the receiver never blocks writing
/// one while the sender is blocked writing data.
pub const ACK_WINDOW: usize = 64;
/// Digests and entries per manifest page, chosen so a page stays well under
/// `MAX_FRAME_BYTES` (32-byte digests, short JSON entries).
pub const MANIFEST_PAGE_ITEMS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferScope {
    /// Under the destination data directory.
    Data,
    /// Under the destination configuration directory.
    Config,
    /// Beside the destination SessionWiki index.
    SessionWiki,
    /// Under the destination user's home directory.
    Home,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbTarget {
    /// `mj.sqlite3`
    Main,
    /// SessionWiki's `index.db`
    SessionWiki,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileKind {
    Regular { size: u64, mode: u32, mtime_ns: i64 },
    Symlink { target: PathBuf },
    Directory { mode: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub mj_version: String,
    pub schema_version: i64,
    pub protocol_version: u32,
    pub os_family: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DestinationStatus {
    Empty,
    ExistingStore {
        has_db: bool,
        has_config: bool,
    },
    ContainerDaemon,
    /// An earlier `CommitPublication` began and did not finish.
    ResumablePublication {
        expected_fence: StoreFenceRecord,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionWikiStatus {
    Absent,
    Existing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub mj_version: String,
    pub schema_version: i64,
    pub protocol_version: u32,
    pub os_family: String,
    pub home_dir: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub sessionwiki_db: PathBuf,
    pub destination_status: DestinationStatus,
    pub sessionwiki_status: SessionWikiStatus,
    pub available_bytes_data: Option<u64>,
    pub available_bytes_config: Option<u64>,
    pub available_bytes_sessionwiki: Option<u64>,
}

/// Probe SSH from the destination, using the OpenSSH configuration and keys
/// effective there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSshRequest {
    /// Avoid writing SSH host-key files during a dry run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    pub source: Option<SourceProbe>,
    pub third_hosts: Vec<mj_core::targets::SshTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProbe {
    pub ssh: mj_core::targets::SshTarget,
    pub data_root: PathBuf,
    pub expected_identity: String,
    pub engines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSshAck {
    /// The user name the destination reached on the source.
    pub source_user: Option<String>,
    /// One message per probe that failed.
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedEntry {
    pub scope: TransferScope,
    pub rel_path: PathBuf,
    pub kind: FileKind,
    /// Present only when the request asked for file hashes.
    pub sha256: Option<String>,
}

/// One page of a staging manifest. Digests, when present, are the 32-byte
/// SHA-256 of consecutive 1 MiB blocks of `target` starting at `first_block`,
/// concatenated in the frame payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPage {
    pub entries: Vec<StagedEntry>,
    pub blocks: Option<BlockPage>,
    pub last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockPage {
    pub target: DbTarget,
    pub total_size: u64,
    pub first_block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeTarget {
    /// Relative to the destination home directory.
    pub rel_path: PathBuf,
    /// A single credential file, published with mode 0600.
    pub private: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPublication {
    pub rewrite_spec: MigrationRewriteSpec,
    pub config_rewrite_spec: StagedConfigRewriteSpec,
    pub home_targets: Vec<HomeTarget>,
    pub migrate_sessionwiki: bool,
    pub overwrite_sessionwiki: bool,
    pub start_daemon: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublicationSummary {
    pub store: StoreRewriteSummary,
    pub home_targets: usize,
    pub data_entries: usize,
    pub sessionwiki: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommitPublicationAck {
    /// The destination store is unfenced. The source must stay fenced.
    Activated {
        summary: PublicationSummary,
        daemon_started: bool,
    },
    /// Nothing was activated; the destination store is still fenced or absent.
    FailedBeforeCommit { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello(Hello),
    ProbePaths {
        paths: Vec<PathBuf>,
    },
    ProbeSsh(ProbeSshRequest),
    StageManifest {
        /// Include the staged file list.
        files: bool,
        /// Compute whole-file SHA-256 for staged regular files.
        hash_files: bool,
        /// Databases whose staged 1 MiB block digests are wanted.
        blocks: Vec<DbTarget>,
    },
    /// Regular files continue with `PutFileChunk` and `PutFileEnd`; symlinks
    /// and directories are complete in this request.
    PutFileStart {
        scope: TransferScope,
        rel_path: PathBuf,
        kind: FileKind,
    },
    /// The payload is zstd-compressed; `raw_len` is its decompressed length.
    PutFileChunk {
        raw_len: u32,
    },
    PutFileEnd {
        sha256: String,
    },
    PatchBlocksBegin {
        target: DbTarget,
        total_size: u64,
    },
    /// The payload is the zstd-compressed block.
    PatchBlock {
        index: u64,
        raw_len: u32,
        sha256: String,
    },
    PatchBlocksEnd,
    RemoveFile {
        scope: TransferScope,
        rel_path: PathBuf,
    },
    CommitPublication(Box<CommitPublication>),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloAck(Box<HelloAck>),
    ProbePathsAck {
        exists: BTreeMap<PathBuf, bool>,
    },
    ProbeSshAck(ProbeSshAck),
    StageManifestAck(ManifestPage),
    Ack,
    CommitPublicationAck(CommitPublicationAck),
    DoneAck,
    /// The request failed and the destination ends the stream.
    Error {
        message: String,
    },
}

/// Reject any relative path that is not made only of plain names: `..`, `.`,
/// root, prefixes and empty paths could all leave the directory a path is
/// meant to stay inside.
pub fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "empty relative path");
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                ensure!(
                    name.to_str().is_some_and(|name| !name.contains('\0')),
                    "path component is not valid text: {}",
                    path.display()
                );
            }
            _ => bail!("path escapes its directory: {}", path.display()),
        }
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

pub fn compress(raw: &[u8]) -> Result<Vec<u8>> {
    zstd::bulk::compress(raw, ZSTD_LEVEL).context("compress transfer chunk")
}

/// Decompress a chunk that must be exactly `raw_len` bytes and at most one block.
pub fn decompress(payload: &[u8], raw_len: u32) -> Result<Vec<u8>> {
    ensure!(
        raw_len as usize <= BLOCK_BYTES,
        "transfer chunk of {raw_len} bytes exceeds the block size"
    );
    let raw =
        zstd::bulk::decompress(payload, raw_len as usize).context("decompress transfer chunk")?;
    ensure!(
        raw.len() == raw_len as usize,
        "transfer chunk decompressed to {} bytes, expected {raw_len}",
        raw.len()
    );
    Ok(raw)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: &impl Serialize,
    payload: &[u8],
) -> Result<()> {
    let header = serde_json::to_vec(header).context("encode frame header")?;
    let length = 4 + header.len() + payload.len();
    ensure!(
        length <= MAX_FRAME_BYTES,
        "frame of {length} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
    );
    writer.write_all(&(length as u32).to_be_bytes()).await?;
    writer
        .write_all(&(header.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    Ok(())
}

/// Read one frame. `Ok(None)` means the stream ended cleanly between frames.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<(T, Vec<u8>)>> {
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read frame length"),
    }
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        (4..=MAX_FRAME_BYTES).contains(&length),
        "frame length {length} is outside 4..={MAX_FRAME_BYTES}"
    );
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .await
        .context("stream ended inside a frame")?;
    let header_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    ensure!(
        header_len <= length - 4,
        "frame header length {header_len} exceeds its frame"
    );
    let header = serde_json::from_slice(&body[4..4 + header_len]).context("decode frame header")?;
    let payload = body.split_off(4 + header_len);
    Ok(Some((header, payload)))
}
