use super::{MAX_SESSION_FILE_BYTES, SessionExportError};
use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

/// The most file content a directory fetch reads before it compresses any of
/// it. The archive itself is held to [`MAX_SESSION_FILE_BYTES`]; this bound
/// stops a directory of very compressible files from costing minutes of reading
/// on the target to produce an archive that would still fit.
pub const MAX_FETCHED_DIRECTORY_BYTES: u64 = 8 * MAX_SESSION_FILE_BYTES;

/// What a caller asked of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    /// A file's bytes, or a zip archive of a directory.
    Content,
    /// A [`SessionListing`] as JSON: a directory's entries, or the file itself.
    Listing,
}

/// What a fetch returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchedKind {
    /// A regular file, returned as it is.
    File,
    /// A directory, returned as a zip archive of everything below it.
    Directory,
    /// A [`SessionListing`] as JSON.
    Listing,
}

/// The entries at a path on a session's target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListing {
    /// The path on the target after symlinks were resolved.
    pub path: String,
    /// A directory's entries in name order, or the one file the path names.
    pub entries: Vec<ListedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedEntry {
    pub name: String,
    pub kind: ListedKind,
    /// The entry's own size in bytes; a symlink's is the length of its target.
    pub size: u64,
    /// The last modification, as RFC 3339 in UTC, when the target records one.
    pub modified: Option<String>,
    /// Where a symlink points. Links are listed, never followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListedKind {
    File,
    Directory,
    Symlink,
    /// A socket, FIFO or device.
    Other,
}

/// One path read from a session's target by [`fetch_session_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPath {
    pub kind: FetchedKind,
    /// The path on the target after symlinks were resolved.
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct FetchedHeader {
    kind: FetchedKind,
    path: String,
}

impl FetchedPath {
    /// The worker's standard output: one JSON line naming the kind and the
    /// resolved path, then the bytes.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut output = serde_json::to_vec(&FetchedHeader {
            kind: self.kind,
            path: self.path.clone(),
        })?;
        output.push(b'\n');
        output.extend_from_slice(&self.bytes);
        Ok(output)
    }

    pub fn decode(mut output: Vec<u8>) -> anyhow::Result<Self> {
        let end = output
            .iter()
            .position(|byte| *byte == b'\n')
            .context("the fetched path has no header line")?;
        let header: FetchedHeader =
            serde_json::from_slice(&output[..end]).context("decode the fetched path header")?;
        let bytes = output.split_off(end + 1);
        Ok(Self {
            kind: header.kind,
            path: header.path,
            bytes,
        })
    }
}

/// Read a file, zip a directory, or list either, when it lies inside one of
/// `roots` or, when `temp` is set, inside this machine's temporary directory.
///
/// Containment is checked after symlinks are resolved, so neither a `..` nor a
/// link can reach the rest of the filesystem. A root that does not exist is
/// skipped: nothing can lie inside it. Inside a directory, symlinks are stored
/// or listed as links and never followed.
pub fn fetch_session_path(
    path: &Path,
    roots: &[PathBuf],
    temp: bool,
    mode: FetchMode,
) -> Result<FetchedPath, SessionExportError> {
    fetch_with_limits(
        path,
        roots,
        temp,
        mode,
        MAX_SESSION_FILE_BYTES,
        MAX_FETCHED_DIRECTORY_BYTES,
    )
}

fn fetch_with_limits(
    path: &Path,
    roots: &[PathBuf],
    temp: bool,
    mode: FetchMode,
    max_output: u64,
    max_content: u64,
) -> Result<FetchedPath, SessionExportError> {
    let mut allowed = roots.to_vec();
    if temp {
        allowed.push(std::env::temp_dir());
        #[cfg(unix)]
        allowed.push(PathBuf::from("/tmp"));
    }
    let mut resolved_roots = Vec::new();
    for root in &allowed {
        match root.canonicalize() {
            Ok(root) => resolved_roots.push(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("resolve {}", root.display()))
                    .into());
            }
        }
    }
    let described = || {
        allowed
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let resolved = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SessionExportError::Refused(format!(
                "{} does not exist on the session's target",
                path.display()
            ))
        } else {
            anyhow::Error::new(error)
                .context(format!("resolve {}", path.display()))
                .into()
        }
    })?;
    if !resolved_roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(SessionExportError::Refused(format!(
            "{} is outside the directories a session shares: {}",
            path.display(),
            described()
        )));
    }
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("inspect {}", resolved.display()))?;
    let display = resolved.to_string_lossy().into_owned();
    if mode == FetchMode::Listing {
        let bytes = list_path(&resolved, &metadata, max_output)?;
        return Ok(FetchedPath {
            kind: FetchedKind::Listing,
            path: display,
            bytes,
        });
    }
    if metadata.is_file() {
        if metadata.len() > max_output {
            return Err(SessionExportError::Refused(format!(
                "{display} is larger than {max_output} bytes"
            )));
        }
        let bytes = fs::read(&resolved).with_context(|| format!("read {display}"))?;
        return Ok(FetchedPath {
            kind: FetchedKind::File,
            path: display,
            bytes,
        });
    }
    if !metadata.is_dir() {
        return Err(SessionExportError::Refused(format!(
            "{display} is neither a regular file nor a directory"
        )));
    }
    let bytes = zip_directory(&resolved, max_output, max_content)?;
    Ok(FetchedPath {
        kind: FetchedKind::Directory,
        path: display,
        bytes,
    })
}

/// A [`SessionListing`] of `path` as JSON, no larger than `max_output`.
fn list_path(
    path: &Path,
    metadata: &fs::Metadata,
    max_output: u64,
) -> Result<Vec<u8>, SessionExportError> {
    let display = path.to_string_lossy().into_owned();
    let entries = if metadata.is_dir() {
        let mut names = fs::read_dir(path)
            .with_context(|| format!("list {display}"))?
            .map(|child| child.map(|child| child.file_name()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("list {display}"))?;
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let child = path.join(&name);
                let metadata = fs::symlink_metadata(&child)
                    .with_context(|| format!("inspect {}", child.display()))?;
                listed_entry(&child, name.to_string_lossy().into_owned(), &metadata)
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| display.clone());
        vec![listed_entry(path, name, metadata)?]
    };
    let bytes = serde_json::to_vec(&SessionListing {
        path: display.clone(),
        entries,
    })
    .context("encode the listing")?;
    if bytes.len() as u64 > max_output {
        return Err(SessionExportError::Refused(format!(
            "{display} has too many entries to list in {max_output} bytes; list a subdirectory"
        )));
    }
    Ok(bytes)
}

fn listed_entry(path: &Path, name: String, metadata: &fs::Metadata) -> anyhow::Result<ListedEntry> {
    let kind = metadata.file_type();
    let (kind, target) = if kind.is_symlink() {
        let target =
            fs::read_link(path).with_context(|| format!("read link {}", path.display()))?;
        (
            ListedKind::Symlink,
            Some(target.to_string_lossy().into_owned()),
        )
    } else if kind.is_dir() {
        (ListedKind::Directory, None)
    } else if kind.is_file() {
        (ListedKind::File, None)
    } else {
        (ListedKind::Other, None)
    };
    Ok(ListedEntry {
        name,
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok().map(|time| {
            chrono::DateTime::<chrono::Utc>::from(time)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
        target,
    })
}

enum Entry {
    Directory,
    File { size: u64 },
    Symlink { target: PathBuf },
}

/// Every entry below `directory`, in name order, keyed by its path relative to
/// the directory's parent so the archive unpacks into one directory.
fn collect_entries(
    directory: &Path,
    name: &Path,
    entries: &mut Vec<(PathBuf, Entry)>,
) -> anyhow::Result<()> {
    entries.push((name.to_path_buf(), Entry::Directory));
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("list {}", directory.display()))?
        .map(|child| child.map(|child| child.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("list {}", directory.display()))?;
    children.sort();
    for child in children {
        let path = directory.join(&child);
        let child_name = name.join(&child);
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            let target =
                fs::read_link(&path).with_context(|| format!("read link {}", path.display()))?;
            entries.push((child_name, Entry::Symlink { target }));
        } else if kind.is_dir() {
            collect_entries(&path, &child_name, entries)?;
        } else if kind.is_file() {
            entries.push((
                child_name,
                Entry::File {
                    size: metadata.len(),
                },
            ));
        }
        // Sockets, FIFOs and devices have no content to archive.
    }
    Ok(())
}

fn zip_directory(
    directory: &Path,
    max_output: u64,
    max_content: u64,
) -> Result<Vec<u8>, SessionExportError> {
    let display = directory.display().to_string();
    let name = directory
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("root"));
    let mut entries = Vec::new();
    collect_entries(directory, &name, &mut entries)?;
    let content: u64 = entries
        .iter()
        .map(|(_, entry)| match entry {
            Entry::File { size } => *size,
            _ => 0,
        })
        .sum();
    if content > max_content {
        return Err(SessionExportError::Refused(format!(
            "{display} holds {content} bytes of files, more than the {max_content} a directory fetch reads; fetch a subdirectory"
        )));
    }
    let too_large = || {
        SessionExportError::Refused(format!(
            "{display} is larger than {max_output} bytes as a zip archive; fetch a subdirectory"
        ))
    };
    let failed = |error: zip::result::ZipError, entry: &Path| -> SessionExportError {
        match error {
            zip::result::ZipError::Io(error) if is_limit(&error) => too_large(),
            error => anyhow!(error)
                .context(format!("archive {}", entry.display()))
                .into(),
        }
    };
    let parent = directory.parent().unwrap_or(directory);
    let mut writer = zip::ZipWriter::new(BoundedBuffer::new(max_output));
    for (entry_name, entry) in &entries {
        // Zip names use `/` whatever the target's separator is.
        let zip_name = entry_name
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let path = parent.join(entry_name);
        match entry {
            Entry::Directory => writer
                .add_directory(zip_name, SimpleFileOptions::default())
                .map_err(|error| failed(error, &path))?,
            Entry::Symlink { target } => writer
                .add_symlink(
                    zip_name,
                    target.to_string_lossy(),
                    SimpleFileOptions::default(),
                )
                .map_err(|error| failed(error, &path))?,
            Entry::File { .. } => {
                let options =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
                #[cfg(unix)]
                let options = {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = fs::metadata(&path)
                        .with_context(|| format!("inspect {}", path.display()))?
                        .permissions()
                        .mode();
                    options.unix_permissions(mode & 0o777)
                };
                writer
                    .start_file(zip_name, options)
                    .map_err(|error| failed(error, &path))?;
                let mut file =
                    fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
                if let Err(error) = std::io::copy(&mut file, &mut writer) {
                    if is_limit(&error) {
                        return Err(too_large());
                    }
                    return Err(anyhow::Error::new(error)
                        .context(format!("archive {}", path.display()))
                        .into());
                }
            }
        }
    }
    let buffer = writer.finish().map_err(|error| failed(error, directory))?;
    Ok(buffer.into_inner())
}

/// The error [`BoundedBuffer`] answers once a write would pass its limit.
#[derive(Debug)]
struct OverLimit;

impl std::fmt::Display for OverLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the archive passed its size limit")
    }
}

impl std::error::Error for OverLimit {}

fn is_limit(error: &std::io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<OverLimit>())
}

/// An in-memory archive that refuses to grow past a limit, so compressing a
/// large directory stops at the limit instead of after the whole directory.
///
/// After the first refusal it discards what it is given: the zip writer
/// finishes the archive when it is dropped, and would otherwise print that
/// second failure to standard error.
struct BoundedBuffer {
    cursor: Cursor<Vec<u8>>,
    limit: u64,
    refused: bool,
}

impl BoundedBuffer {
    fn new(limit: u64) -> Self {
        Self {
            cursor: Cursor::new(Vec::new()),
            limit,
            refused: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.cursor.into_inner()
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.refused {
            return Ok(buffer.len());
        }
        if self.cursor.position() + buffer.len() as u64 > self.limit {
            self.refused = true;
            return Err(std::io::Error::other(OverLimit));
        }
        self.cursor.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for BoundedBuffer {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.cursor.seek(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn refusal(result: Result<FetchedPath, SessionExportError>) -> String {
        match result {
            Err(SessionExportError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fetch_stays_inside_the_shared_roots_and_archives_links_without_following_them() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"private\n").unwrap();
        let evidence = workspace.path().join("evidence");
        fs::create_dir_all(evidence.join("nested/empty")).unwrap();
        // More than one pipe buffer, so the archive is not a toy.
        let log: Vec<u8> = (0..200_000u32).flat_map(|n| n.to_le_bytes()).collect();
        fs::write(evidence.join("nested/probe.log"), &log).unwrap();
        fs::write(evidence.join("report.md"), b"# Report\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), evidence.join("leak"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape")).unwrap();
        let roots = [workspace.path().to_path_buf()];

        // A link that resolves outside every root is refused, whether it names
        // the file or a directory above it.
        for path in [
            evidence.join("leak"),
            workspace.path().join("escape/secret.txt"),
            workspace
                .path()
                .join("../")
                .join(outside.path().file_name().unwrap()),
        ] {
            let reason = refusal(fetch_session_path(&path, &roots, false, FetchMode::Content));
            assert!(
                reason.contains("outside the directories a session shares"),
                "{}: {reason}",
                path.display()
            );
        }

        let fetched = fetch_session_path(&evidence, &roots, false, FetchMode::Content).unwrap();
        assert_eq!(fetched.kind, FetchedKind::Directory);
        assert_eq!(
            fetched,
            FetchedPath::decode(fetched.encode().unwrap()).unwrap(),
            "the worker's framing carries the archive intact"
        );
        let mut archive = zip::ZipArchive::new(Cursor::new(fetched.bytes)).unwrap();
        let names: Vec<_> = archive.file_names().map(str::to_owned).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            [
                "evidence/",
                "evidence/leak",
                "evidence/nested/",
                "evidence/nested/empty/",
                "evidence/nested/probe.log",
                "evidence/report.md",
            ]
        );
        let mut content = Vec::new();
        archive
            .by_name("evidence/nested/probe.log")
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, log);
        // The link is stored as a link: its entry holds the target's name,
        // never the secret it points at.
        let mut link = String::new();
        let mut entry = archive.by_name("evidence/leak").unwrap();
        assert!(entry.is_symlink());
        entry.read_to_string(&mut link).unwrap();
        assert_eq!(link, outside.path().join("secret.txt").to_string_lossy());
    }

    #[test]
    fn a_directory_fetch_stops_at_its_limits_instead_of_after_the_whole_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let directory = workspace.path().join("probes");
        fs::create_dir(&directory).unwrap();
        // Incompressible, so the archive grows as fast as the input.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..300_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        fs::write(directory.join("noise.bin"), &noise).unwrap();
        let roots = [workspace.path().to_path_buf()];

        let reason = refusal(fetch_with_limits(
            &directory,
            &roots,
            false,
            FetchMode::Content,
            100_000,
            1_000_000,
        ));
        assert!(reason.contains("as a zip archive"), "{reason}");
        let reason = refusal(fetch_with_limits(
            &directory,
            &roots,
            false,
            FetchMode::Content,
            1_000_000,
            100_000,
        ));
        assert!(reason.contains("bytes of files"), "{reason}");
        assert_eq!(
            fetch_with_limits(
                &directory,
                &roots,
                false,
                FetchMode::Content,
                1_000_000,
                1_000_000
            )
            .unwrap()
            .kind,
            FetchedKind::Directory
        );
    }
}
