//! The logins an OpenCode bridge imported when it started.
//!
//! Staging and the credential sync keep the profile's logins in the session
//! home as the export `opencode auth import` reads
//! ([`HarnessKind::credential_file_name`]), and every launch imports it
//! ([`mj_core::harness_runtime::opencode_acp_script`]). The session's own
//! database only copies those logins: each bridge starts with its copy
//! emptied so the import makes it exactly the staged export, and an idle
//! session whose staged export has changed reloads on a fresh bridge, which
//! imports it. A running OpenCode reloads its providers the same way when a
//! login changes through its own server, which Mjolnir cannot reach.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::config::HarnessKind;
use serde_json::Value;

/// How often an idle OpenCode session looks for changed logins.
#[cfg(not(test))]
pub(super) const CHECK_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const CHECK_INTERVAL: Duration = Duration::from_millis(20);

/// The staged export a bridge's launch imported, as it read it.
#[derive(Debug, Clone)]
pub struct ImportedLogins {
    path: PathBuf,
    entries: Vec<Value>,
}

impl ImportedLogins {
    /// Before an OpenCode bridge starts in `environment`: empty the session
    /// database's logins, so the launch's import makes them exactly the staged
    /// ones, and record the export that import reads. Recorded before the
    /// bridge reads it, so a change made in between is seen as a change.
    pub(super) fn prepare(environment: &BTreeMap<String, String>) -> Result<Self> {
        let kind = HarnessKind::OpenCode;
        let variable = |name: &str| {
            environment
                .get(name)
                .map(PathBuf::from)
                .with_context(|| format!("the OpenCode launch sets no {name}"))
        };
        let path = kind
            .home_from_environment(variable(kind.home_env())?)
            .join(kind.credential_file_name());
        let database = variable("XDG_DATA_HOME")?
            .join(kind.id())
            .join("opencode.db");
        clear_database_logins(&database)?;
        Ok(Self {
            entries: read_entries(&path)?,
            path,
        })
    }

    /// Whether the staged export now differs from the one this bridge
    /// imported. A missing export is the same as one without logins.
    pub(super) async fn changed(&self) -> Result<bool> {
        let path = self.path.clone();
        let entries = tokio::task::spawn_blocking(move || read_entries(&path))
            .await
            .context("read the staged OpenCode logins")??;
        Ok(entries != self.entries)
    }
}

fn read_entries(path: &Path) -> Result<Vec<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("decode the staged OpenCode logins {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error)
            .with_context(|| format!("read the staged OpenCode logins {}", path.display())),
    }
}

/// Delete every login from the session database at `path`, which holds only
/// copies of the staged ones. The previous bridge has been stopped before
/// this one is prepared, and its server child ends with it; anything that
/// child might still write about a login updates its row, and cannot bring a
/// deleted one back.
fn clear_database_logins(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let clear = || -> Result<()> {
        let database = rusqlite::Connection::open(path)?;
        database.busy_timeout(Duration::from_secs(5))?;
        let exists: bool = database.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'credential')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            database.execute("DELETE FROM credential", [])?;
        }
        Ok(())
    };
    clear().with_context(|| format!("clear the OpenCode session's logins in {}", path.display()))
}
