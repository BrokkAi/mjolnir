//! Streaming snapshots of directories attached to remote session targets.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path};

use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
#[cfg(unix)]
use flate2::{Compression, write::GzEncoder};

const MAX_ENTRIES: usize = 1_000_000;
const MAX_EXPANDED_BYTES: u64 = 50 * 1024 * 1024 * 1024;

/// Produce a tar.gz stream without materializing it on disk or in memory.
#[cfg(unix)]
pub fn stream_resource(
    source: &Path,
    consume: impl FnOnce(&mut (dyn Read + Send)) -> Result<()>,
) -> Result<()> {
    use std::os::unix::net::UnixStream;

    ensure!(
        source.is_dir(),
        "resource source is not a directory: {}",
        source.display()
    );
    let (mut reader, writer) = UnixStream::pair().context("create resource stream")?;
    let source = source.to_path_buf();
    let producer = std::thread::spawn(move || write_resource_stream(&source, writer));
    let consumed = consume(&mut reader);
    drop(reader);
    let produced = producer
        .join()
        .map_err(|_| anyhow::anyhow!("resource compression thread panicked"))?;
    match (consumed, produced) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

#[cfg(not(unix))]
pub fn stream_resource(
    _source: &Path,
    _consume: impl FnOnce(&mut (dyn Read + Send)) -> Result<()>,
) -> Result<()> {
    anyhow::bail!("streaming attached resources requires a Unix controller")
}

#[cfg(unix)]
fn write_resource_stream(source: &Path, output: impl Write) -> Result<()> {
    let encoder = GzEncoder::new(output, Compression::fast());
    let mut archive = tar::Builder::new(encoder);
    let mut entries = 0;
    append_directory(&mut archive, source, source, &mut entries)?;
    let encoder = archive.into_inner().context("finish resource tar stream")?;
    encoder.finish().context("finish resource gzip stream")?;
    Ok(())
}

#[cfg(unix)]
fn append_directory<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    directory: &Path,
    entries: &mut usize,
) -> Result<()> {
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("read resource directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    if children.is_empty() && directory != root {
        ensure_entry_limit(entries)?;
        let relative = safe_relative_path(root, directory)?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive.append_data(&mut header, relative, std::io::empty())?;
        *entries += 1;
    }
    for child in children {
        ensure_entry_limit(entries)?;
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "resource contains unsupported symbolic link: {}",
            path.display()
        );
        if metadata.is_dir() {
            append_directory(archive, root, &path, entries)?;
            continue;
        }
        ensure!(
            metadata.is_file(),
            "resource entry is not a regular file: {}",
            path.display()
        );
        let relative = safe_relative_path(root, &path)?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&metadata);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_cksum();
        archive.append_data(&mut header, relative, File::open(&path)?)?;
        *entries += 1;
    }
    Ok(())
}

fn ensure_entry_limit(entries: &usize) -> Result<()> {
    ensure!(
        *entries < MAX_ENTRIES,
        "resource archive has too many entries"
    );
    Ok(())
}

#[cfg(unix)]
fn safe_relative_path<'a>(root: &Path, path: &'a Path) -> Result<&'a Path> {
    let relative = path.strip_prefix(root)?;
    ensure!(
        !relative.as_os_str().is_empty(),
        "resource archive entry is empty"
    );
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe resource archive path: {}",
        relative.display()
    );
    Ok(relative)
}

pub fn install_resource_stream(input: impl Read, destination: &Path) -> Result<()> {
    validate_destination(destination)?;
    fs::create_dir_all(destination)
        .with_context(|| format!("create resource destination {}", destination.display()))?;
    let decoder = GzDecoder::new(input);
    let mut archive = tar::Archive::new(decoder);
    let mut expanded = 0_u64;
    for (entry_index, entry) in archive
        .entries()
        .context("read resource tar stream")?
        .enumerate()
    {
        ensure_entry_limit(&entry_index)?;
        let mut entry = entry.context("read resource tar entry")?;
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "resource stream contains unsupported entry type"
        );
        let relative = entry.path().context("read resource entry path")?;
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "unsafe resource archive path: {}",
            relative.display()
        );
        expanded = expanded
            .checked_add(entry.size())
            .context("resource expanded size overflow")?;
        ensure!(
            expanded <= MAX_EXPANDED_BYTES,
            "resource expanded size exceeds 50 GiB"
        );
        let output = destination.join(relative.as_ref());
        ensure_safe_parent(destination, output.parent().unwrap_or(destination))?;
        ensure_not_symlink(&output)?;
        if kind.is_dir() {
            fs::create_dir_all(&output)?;
        } else {
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(&output)?;
            std::io::copy(&mut entry, &mut file)?;
            file.flush()?;
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
    }
    Ok(())
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "resource destination is a symbolic link: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_destination(destination: &Path) -> Result<()> {
    ensure!(
        destination.is_absolute(),
        "resource destination must be absolute"
    );
    ensure!(
        destination != Path::new("/"),
        "resource destination cannot be the filesystem root"
    );
    ensure!(
        !destination
            .components()
            .any(|part| part == Component::ParentDir),
        "resource destination cannot contain '..'"
    );
    Ok(())
}

fn ensure_safe_parent(root: &Path, parent: &Path) -> Result<()> {
    let relative = parent.strip_prefix(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "resource destination traverses symbolic link: {}",
                current.display()
            );
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn resource_directory_round_trips_through_one_stream() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("many/nested")).unwrap();
        fs::write(source.path().join("many/nested/a.txt"), b"alpha").unwrap();
        fs::create_dir_all(source.path().join("empty")).unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let destination = destination_root.path().join("installed");

        stream_resource(source.path(), |stream| {
            install_resource_stream(stream, &destination)
        })
        .unwrap();

        assert_eq!(
            fs::read(destination.join("many/nested/a.txt")).unwrap(),
            b"alpha"
        );
        assert!(destination.join("empty").is_dir());
    }

    #[test]
    fn resource_streams_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        symlink("outside", source.path().join("link")).unwrap();
        let error = stream_resource(source.path(), |stream| {
            std::io::copy(stream, &mut std::io::sink())?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }
}
