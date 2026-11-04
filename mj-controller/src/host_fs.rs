//! The daemon's handle on the user's files.
//!
//! In a `mj daemon-run` process those files are on the node's computer, not
//! beside the daemon. The node registry publishes its connection here, and
//! the helpers in this module either stream the operation through it or run
//! it locally when no command host is installed (tests and in-process
//! daemons). Daemon data, such as the store, worker roots and checkpoint
//! archives, is never read through this handle.

// Async and blocking forms are both part of the handle: each call site uses
// the one its context allows while the host-file conversion proceeds.
#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use mj_core::node::{FsEntry, FsMetadata, NODE_CONNECT_WAIT, NodeClient};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// One process's handle on the host filesystem, installed by the daemon.
pub struct HostFs {
    slot: watch::Receiver<Option<NodeClient>>,
    runtime: tokio::runtime::Handle,
    shutdown: CancellationToken,
}

static HOST_FS: OnceLock<Arc<HostFs>> = OnceLock::new();

/// Install the daemon's host filesystem. A second install is a programming
/// error, not a second answer.
pub fn install(
    slot: watch::Receiver<Option<NodeClient>>,
    runtime: tokio::runtime::Handle,
    shutdown: CancellationToken,
) -> Result<()> {
    let host = Arc::new(HostFs {
        slot,
        runtime,
        shutdown,
    });
    HOST_FS
        .set(host)
        .map_err(|_| anyhow::anyhow!("the daemon host filesystem is already installed"))
}

fn installed() -> Option<&'static Arc<HostFs>> {
    HOST_FS.get()
}

impl HostFs {
    async fn connected(&self) -> Result<NodeClient> {
        let mut slot = self.slot.clone();
        let connected = async {
            loop {
                if let Some(client) = slot.borrow_and_update().clone() {
                    return Ok(client);
                }
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => {
                        bail!("daemon shutdown cancelled the wait for a node connection");
                    }
                    changed = slot.changed() => {
                        changed.context("the daemon's node registry has stopped")?;
                    }
                }
            }
        };
        match tokio::time::timeout(NODE_CONNECT_WAIT, connected).await {
            Ok(result) => result,
            Err(_) => bail!(
                "no node from this computer is connected, so its files are unavailable; \
                 `mj` starts one beside the daemon"
            ),
        }
    }

    async fn read_async(&self, path: &Path) -> Result<Vec<u8>> {
        self.connected()
            .await?
            .fs_read(path)
            .await
            .with_context(|| format!("read {} through the node", path.display()))
    }

    async fn write_async(&self, path: &Path, data: Vec<u8>) -> Result<()> {
        self.connected()
            .await?
            .fs_write(path, data.into())
            .await
            .with_context(|| format!("write {} through the node", path.display()))
    }

    async fn list_async(&self, path: &Path) -> Result<Vec<FsEntry>> {
        self.connected()
            .await?
            .fs_list(path)
            .await
            .with_context(|| format!("list {} through the node", path.display()))
    }

    async fn metadata_async(&self, path: &Path) -> Result<FsMetadata> {
        self.connected()
            .await?
            .fs_metadata(path)
            .await
            .with_context(|| format!("inspect {} through the node", path.display()))
    }

    fn read_sync(&self, path: &Path) -> Result<Vec<u8>> {
        self.runtime.block_on(self.read_async(path))
    }

    fn write_sync(&self, path: &Path, data: Vec<u8>) -> Result<()> {
        self.runtime.block_on(self.write_async(path, data))
    }

    fn list_sync(&self, path: &Path) -> Result<Vec<FsEntry>> {
        self.runtime.block_on(self.list_async(path))
    }

    fn metadata_sync(&self, path: &Path) -> Result<FsMetadata> {
        self.runtime.block_on(self.metadata_async(path))
    }
}

/// Read a host file. The node serves it when the daemon installed a host
/// filesystem; otherwise this process does.
pub async fn read(path: &Path) -> Result<Vec<u8>> {
    match installed() {
        Some(host) => host.read_async(path).await,
        None => tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display())),
    }
}

/// Read a host file from a blocking thread.
pub fn read_sync(path: &Path) -> Result<Vec<u8>> {
    match installed() {
        Some(host) => host.read_sync(path),
        None => std::fs::read(path).with_context(|| format!("read {}", path.display())),
    }
}

/// Read a host file as UTF-8 text.
pub async fn read_to_string(path: &Path) -> Result<String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes).with_context(|| format!("decode {} as UTF-8", path.display()))
}

/// Read a host file as UTF-8 text from a blocking thread.
pub fn read_to_string_sync(path: &Path) -> Result<String> {
    let bytes = read_sync(path)?;
    String::from_utf8(bytes).with_context(|| format!("decode {} as UTF-8", path.display()))
}

/// Write a host file.
pub async fn write(path: &Path, data: Vec<u8>) -> Result<()> {
    match installed() {
        Some(host) => host.write_async(path, data).await,
        None => tokio::fs::write(path, data)
            .await
            .with_context(|| format!("write {}", path.display())),
    }
}

/// Write a host file from a blocking thread.
pub fn write_sync(path: &Path, data: Vec<u8>) -> Result<()> {
    match installed() {
        Some(host) => host.write_sync(path, data),
        None => std::fs::write(path, data).with_context(|| format!("write {}", path.display())),
    }
}

/// List a host directory.
pub async fn list(path: &Path) -> Result<Vec<FsEntry>> {
    match installed() {
        Some(host) => host.list_async(path).await,
        None => {
            let mut directory = tokio::fs::read_dir(path)
                .await
                .with_context(|| format!("list {}", path.display()))?;
            let mut entries = Vec::new();
            while let Some(entry) = directory
                .next_entry()
                .await
                .with_context(|| format!("list {}", path.display()))?
            {
                let file_type = entry.file_type().await?;
                entries.push(FsEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: if file_type.is_dir() {
                        mj_core::node::FsEntryKind::Directory
                    } else if file_type.is_file() {
                        mj_core::node::FsEntryKind::File
                    } else if file_type.is_symlink() {
                        mj_core::node::FsEntryKind::Symlink
                    } else {
                        mj_core::node::FsEntryKind::Other
                    },
                });
            }
            Ok(entries)
        }
    }
}

/// List a host directory from a blocking thread.
pub fn list_sync(path: &Path) -> Result<Vec<FsEntry>> {
    match installed() {
        Some(host) => host.list_sync(path),
        None => {
            let mut entries = Vec::new();
            for entry in
                std::fs::read_dir(path).with_context(|| format!("list {}", path.display()))?
            {
                let entry = entry?;
                let file_type = entry.file_type()?;
                entries.push(FsEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind: if file_type.is_dir() {
                        mj_core::node::FsEntryKind::Directory
                    } else if file_type.is_file() {
                        mj_core::node::FsEntryKind::File
                    } else if file_type.is_symlink() {
                        mj_core::node::FsEntryKind::Symlink
                    } else {
                        mj_core::node::FsEntryKind::Other
                    },
                });
            }
            Ok(entries)
        }
    }
}

/// Inspect a host path.
pub async fn metadata(path: &Path) -> Result<FsMetadata> {
    match installed() {
        Some(host) => host.metadata_async(path).await,
        None => local_metadata(path).await,
    }
}

/// Inspect a host path from a blocking thread.
pub fn metadata_sync(path: &Path) -> Result<FsMetadata> {
    match installed() {
        Some(host) => host.metadata_sync(path),
        None => local_metadata_sync(path),
    }
}

async fn local_metadata(path: &Path) -> Result<FsMetadata> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("inspect {}", path.display()))?;
    Ok(metadata_from(&metadata))
}

fn local_metadata_sync(path: &Path) -> Result<FsMetadata> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    Ok(metadata_from(&metadata))
}

fn metadata_from(metadata: &std::fs::Metadata) -> FsMetadata {
    FsMetadata {
        len: metadata.len(),
        modified_ms: metadata.modified().ok().and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        }),
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        readonly: metadata.permissions().readonly(),
    }
}
