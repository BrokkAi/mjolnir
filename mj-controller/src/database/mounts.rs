use super::*;

/// Atomically apply the controller's MRU policy for newly used mount sources.
pub fn remember_mount_sources(host: &str, mounts: &[AdditionalMount]) -> Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    let host = host.to_owned();
    let sources = mounts
        .iter()
        .map(|mount| mount.source.clone())
        .collect::<Vec<_>>();
    submit_database_write("remember_mount_sources", move |_| {
        remember_sources(&database_path(), &host, sources)
    })
}

pub fn replace_mount_history(host: &str, sources: &[PathBuf]) -> Result<()> {
    let host = host.to_owned();
    let sources = sources.to_vec();
    submit_database_write("replace_mount_history", move |_| {
        replace_mount_history_in(&database_path(), &host, &sources)
    })
}

pub(super) fn replace_mount_history_in(path: &Path, host: &str, sources: &[PathBuf]) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    write_mount_history(&tx, host, sources)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn write_mount_history(
    tx: &Transaction<'_>,
    host: &str,
    sources: &[PathBuf],
) -> Result<()> {
    tx.execute("DELETE FROM mount_history WHERE host = ?1", [host])?;
    let mut written = Vec::new();
    for source in sources.iter().take(20) {
        if written.contains(source) {
            continue;
        }
        tx.execute(
            "INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
            params![host, path_to_blob(source), written.len() as i64],
        )?;
        written.push(source.clone());
    }
    Ok(())
}

pub(super) fn write_host_container_size(
    tx: &Transaction<'_>,
    host: &str,
    size: HostContainerSize,
) -> Result<()> {
    ensure!(!host.trim().is_empty(), "container size host is empty");
    let cpus = i64::try_from(size.cpus).context("container CPU count exceeds SQLite range")?;
    let memory =
        i64::try_from(size.memory_bytes).context("container memory exceeds SQLite range")?;
    ensure!(
        cpus > 0 && memory > 0,
        "container size values must be positive"
    );
    tx.execute(
        "INSERT INTO host_container_sizes(host, cpus, memory_bytes)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(host) DO UPDATE SET cpus = excluded.cpus, memory_bytes = excluded.memory_bytes",
        params![host, cpus, memory],
    )?;
    Ok(())
}

pub fn remember_project_directory(host: &str, directory: &Path) -> Result<()> {
    let host = format!("project:{host}");
    let directory = directory.to_path_buf();
    submit_database_write("remember_project_directory", move |_| {
        remember_sources(&database_path(), &host, std::iter::once(directory))
    })
}

pub(super) fn remember_sources(
    path: &Path,
    host: &str,
    new_sources: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut sources = {
        let mut statement =
            tx.prepare("SELECT source FROM mount_history WHERE host = ?1 ORDER BY ordinal")?;
        statement
            .query_map([host], |row| Ok(blob_to_path(row.get_ref(0)?.as_blob()?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let additions = new_sources.into_iter().collect::<Vec<_>>();
    for source in additions.iter().rev() {
        sources.retain(|existing| existing != source);
        sources.insert(0, source.clone());
    }
    sources.truncate(20);
    write_mount_history(&tx, host, &sources)?;
    tx.commit()?;
    Ok(())
}
