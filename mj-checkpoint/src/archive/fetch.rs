use super::SessionExportError;
use anyhow::{Context, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

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
#[derive(Debug, Clone)]
pub struct FetchedPath {
    pub kind: FetchedKind,
    /// The path on the target after symlinks were resolved.
    pub path: String,
    payload: Arc<fs::File>,
    length: u64,
    offset: u64,
}

/// A fetched copy on the calling machine, without its content in the response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedFetch {
    pub path: PathBuf,
    pub kind: FetchedKind,
    pub source: String,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct FetchedHeader {
    kind: FetchedKind,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    length: Option<u64>,
}

impl FetchedPath {
    /// Keep the source's whole path below `<temp>/mjolnir/<session>` so equal
    /// filenames from different paths or sessions cannot overwrite each other.
    pub fn local_path(&self, temp: &Path, session: &str) -> anyhow::Result<PathBuf> {
        local_fetch_path(self.kind, &self.path, temp, session)
    }

    pub fn from_file(kind: FetchedKind, path: String, file: fs::File) -> anyhow::Result<Self> {
        let length = file.metadata().context("inspect fetched payload")?.len();
        Ok(Self {
            kind,
            path,
            payload: Arc::new(file),
            length,
            offset: 0,
        })
    }

    pub fn from_reader(
        kind: FetchedKind,
        path: String,
        mut reader: impl Read,
        mut file: fs::File,
    ) -> anyhow::Result<Self> {
        std::io::copy(&mut reader, &mut file).context("spool fetched payload")?;
        Self::from_file(kind, path, file)
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Readers have independent positions even when their file handle is shared.
    pub fn reader(&self) -> impl Read + Seek + Send + 'static {
        super::ArchiveFile {
            file: self.payload.clone(),
            len: self.length,
            position: 0,
            start: self.offset,
            cancelled: None,
        }
    }

    pub fn save(&self, temp: &Path, session: &str) -> anyhow::Result<SavedFetch> {
        Self::store(
            self.kind,
            self.path.clone(),
            temp,
            session,
            self.length,
            self.reader(),
        )
    }

    /// Stage on the destination filesystem and publish only a complete transfer.
    pub fn store(
        kind: FetchedKind,
        source: String,
        temp: &Path,
        session: &str,
        expected: u64,
        mut reader: impl Read,
    ) -> anyhow::Result<SavedFetch> {
        let path = local_fetch_path(kind, &source, temp, session)?;
        let parent = path.parent().context("fetch destination has no parent")?;
        fs::create_dir_all(parent).context("create fetch destination")?;
        // One lock owns this destination's staging path, including across
        // killed worker processes. Never remove the lock inode: another owner
        // may already be waiting on it. Unrelated destinations stay concurrent.
        let key = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join(format!(".mj-fetch-{key}.lock")))
            .context("open fetch destination lock")?;
        lock.lock().context("lock fetch destination")?;
        let _lock = mj_core::file_lock::FileLock::new(lock);
        let stage_path = parent.join(format!(".mj-fetch-{key}.part"));
        // The lock proves the preceding writer has stopped. Reclaim its partial
        // file before reading a retried payload, so restarts cannot accumulate it.
        match fs::remove_file(&stage_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("reclaim interrupted fetch"),
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut staged = StagedFetch {
            file: Some(
                options
                    .open(&stage_path)
                    .context("stage fetch destination")?,
            ),
            path: stage_path,
        };
        let received = std::io::copy(&mut reader, staged.file.as_mut().unwrap())
            .context("write fetched payload")?;
        ensure!(
            received == expected,
            "incomplete fetch transfer: expected {expected} bytes, received {received}"
        );
        mj_core::test_hooks::reach_test_hook("fetch_destination_ready")?;
        staged
            .file
            .take()
            .unwrap()
            .sync_all()
            .context("sync fetched payload")?;
        fs::rename(&staged.path, &path).context("publish fetched payload")?;
        mj_core::config::sync_directory(parent)?;
        Ok(SavedFetch {
            path,
            kind,
            source,
            bytes: received,
        })
    }

    /// Metadata framing is unchanged: a JSON line followed by payload bytes.
    pub fn write_to(&self, mut writer: impl Write) -> anyhow::Result<()> {
        serde_json::to_writer(
            &mut writer,
            &FetchedHeader {
                kind: self.kind,
                path: self.path.clone(),
                length: Some(self.length),
            },
        )?;
        writer.write_all(b"\n")?;
        std::io::copy(&mut self.reader(), &mut writer).context("stream fetched payload")?;
        writer.flush().context("flush fetched payload")
    }

    /// Keep the wire payload in its original anonymous file. The metadata line
    /// is a prefix, not another copy of the artifact to allocate or rewrite.
    pub fn read_from(file: fs::File) -> anyhow::Result<Self> {
        let mut reader = std::io::BufReader::new(file);
        reader.rewind()?;
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line)? != 0,
            "the fetched path has no header line"
        );
        let header: FetchedHeader =
            serde_json::from_str(&line).context("decode fetched path header")?;
        let offset = reader.stream_position()?;
        let file = reader.into_inner();
        let length = file
            .metadata()?
            .len()
            .checked_sub(offset)
            .context("fetch header exceeds payload")?;
        if let Some(expected) = header.length {
            ensure!(
                length == expected,
                "incomplete fetch transfer: expected {expected} bytes, received {length}"
            );
        }
        Ok(Self {
            kind: header.kind,
            path: header.path,
            payload: Arc::new(file),
            length,
            offset,
        })
    }
}

/// Internal transfer spools belong to the instance's data filesystem, not
/// `/tmp`, which is commonly tmpfs. The final user copy keeps its requested
/// destination; only rebuildable transport scratch lives here.
pub fn create_fetch_spool() -> anyhow::Result<fs::File> {
    let directory = mj_core::config::data_dir().join("fetch-spool");
    fs::create_dir_all(&directory).context("create fetch spool directory")?;
    tempfile::tempfile_in(directory).context("create fetch spool")
}

struct StagedFetch {
    file: Option<fs::File>,
    path: PathBuf,
}
impl Drop for StagedFetch {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path=%self.path.display(), %error, "could not remove staged fetch");
        }
    }
}

fn local_fetch_path(
    kind: FetchedKind,
    source: &str,
    temp: &Path,
    session: &str,
) -> anyhow::Result<PathBuf> {
    let mut components = Path::new(session).components();
    ensure!(
        matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(_)), None)
        ),
        "session id {session:?} cannot name a directory"
    );
    let mut local = temp.join("mjolnir").join(session);
    let mut named = false;
    for component in Path::new(source).components() {
        if let std::path::Component::Normal(part) = component {
            local.push(part);
            named = true;
        }
    }
    ensure!(named, "the target answered no path for the fetch");
    if kind == FetchedKind::Directory {
        let mut name = local.file_name().unwrap_or_default().to_os_string();
        name.push(".zip");
        local.set_file_name(name);
    }
    Ok(local)
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
    mut payload: fs::File,
) -> Result<FetchedPath, SessionExportError> {
    let mut allowed = roots.to_vec();
    if temp {
        allowed.push(std::env::temp_dir());
        #[cfg(unix)]
        allowed.push(PathBuf::from("/tmp"));
    }
    // The temporary directory is usually `/tmp` itself; a refusal names each
    // directory once.
    let mut seen = std::collections::HashSet::new();
    allowed.retain(|root| seen.insert(root.clone()));
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
    let kind = if mode == FetchMode::Listing {
        list_path(&resolved, &metadata, &mut payload)?;
        FetchedKind::Listing
    } else if metadata.is_file() {
        let mut file = fs::File::open(&resolved).with_context(|| format!("open {display}"))?;
        std::io::copy(&mut file, &mut payload).with_context(|| format!("read {display}"))?;
        FetchedKind::File
    } else if metadata.is_dir() {
        zip_directory(&resolved, &mut payload)?;
        FetchedKind::Directory
    } else {
        return Err(SessionExportError::Refused(format!(
            "{display} is neither a regular file nor a directory"
        )));
    };
    Ok(FetchedPath::from_file(kind, display, payload)?)
}

/// A [`SessionListing`] of `path` as JSON.
fn list_path(
    path: &Path,
    metadata: &fs::Metadata,
    writer: impl Write,
) -> Result<(), SessionExportError> {
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
    serde_json::to_writer(
        writer,
        &SessionListing {
            path: display.clone(),
            entries,
        },
    )
    .context("encode the listing")?;
    Ok(())
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
    File,
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
            entries.push((child_name, Entry::File));
        }
        // Sockets, FIFOs and devices have no content to archive.
    }
    Ok(())
}

fn zip_directory(directory: &Path, output: impl Write + Seek) -> Result<(), SessionExportError> {
    let name = directory
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("root"));
    let mut entries = Vec::new();
    collect_entries(directory, &name, &mut entries)?;
    let failed = |error: zip::result::ZipError, entry: &Path| -> SessionExportError {
        anyhow!(error)
            .context(format!("archive {}", entry.display()))
            .into()
    };
    let parent = directory.parent().unwrap_or(directory);
    let mut writer = zip::ZipWriter::new(output);
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
            Entry::File => {
                // ZIP64 also handles files that grow past 4 GiB while being read.
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .large_file(true);
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
                    return Err(anyhow::Error::new(error)
                        .context(format!("archive {}", path.display()))
                        .into());
                }
            }
        }
    }
    writer.finish().map_err(|error| failed(error, directory))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetched_path_lands_under_its_session_and_cannot_leave_it() {
        let fetched_location =
            |temp: &Path, session: &str, fetched: &FetchedPath| fetched.local_path(temp, session);
        let temp = PathBuf::from("/scratch");
        let fetched = |kind, path: &str| {
            FetchedPath::from_reader(
                kind,
                path.to_owned(),
                std::io::empty(),
                tempfile::tempfile().unwrap(),
            )
            .unwrap()
        };
        // Two children's reports share a name; the target path keeps them apart.
        assert_eq!(
            fetched_location(
                &temp,
                "d8443f17",
                &fetched(
                    FetchedKind::File,
                    "/workspace/d8443f17/.mj-agents/c1/report.md"
                ),
            )
            .unwrap(),
            PathBuf::from("/scratch/mjolnir/d8443f17/workspace/d8443f17/.mj-agents/c1/report.md")
        );
        assert_eq!(
            fetched_location(
                &temp,
                "d8443f17",
                &fetched(FetchedKind::Directory, "/tmp/rust-veto-cf")
            )
            .unwrap(),
            PathBuf::from("/scratch/mjolnir/d8443f17/tmp/rust-veto-cf.zip")
        );
        // Whatever the target answers, the file stays in the session's directory.
        assert_eq!(
            fetched_location(
                &temp,
                "s",
                &fetched(FetchedKind::File, "/a/../../etc/passwd")
            )
            .unwrap(),
            PathBuf::from("/scratch/mjolnir/s/a/etc/passwd")
        );
        for session in ["../s", "a/b", "/s", ".."] {
            assert!(
                fetched_location(&temp, session, &fetched(FetchedKind::File, "/x")).is_err(),
                "{session:?} must not name a directory"
            );
        }
    }

    #[cfg(unix)]
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
            let reason = refusal(fetch_session_path(
                &path,
                &roots,
                false,
                FetchMode::Content,
                tempfile::tempfile().unwrap(),
            ));
            assert!(
                reason.contains("outside the directories a session shares"),
                "{}: {reason}",
                path.display()
            );
        }

        let fetched = fetch_session_path(
            &evidence,
            &roots,
            false,
            FetchMode::Content,
            tempfile::tempfile().unwrap(),
        )
        .unwrap();
        assert_eq!(fetched.kind, FetchedKind::Directory);
        let mut wire = tempfile::tempfile().unwrap();
        fetched.write_to(&mut wire).unwrap();
        wire.rewind().unwrap();
        let decoded = FetchedPath::read_from(wire).unwrap();
        assert_eq!(decoded.kind, fetched.kind);
        assert_eq!(decoded.path, fetched.path);
        let mut archive = zip::ZipArchive::new(decoded.reader()).unwrap();
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

    // Hard-won: #1543: fetch rejected real artifacts and directories at arbitrary size limits.
    #[test]
    fn fetch_transfers_files_and_archives_beyond_the_former_size_limits() {
        let workspace = tempfile::tempdir().unwrap();
        let directory = workspace.path().join("probes");
        fs::create_dir(&directory).unwrap();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..17 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let file = directory.join("noise.bin");
        fs::write(&file, &noise).unwrap();
        let roots = [workspace.path().to_path_buf()];
        let fetched = fetch_session_path(
            &file,
            &roots,
            false,
            FetchMode::Content,
            tempfile::tempfile().unwrap(),
        )
        .unwrap();
        let mut restored = Vec::new();
        fetched.reader().read_to_end(&mut restored).unwrap();
        assert_eq!(restored, noise);
        // This compresses well but exceeds the former directory input limit.
        fs::File::create(directory.join("zeros.bin"))
            .unwrap()
            .set_len(129 * 1024 * 1024)
            .unwrap();
        let fetched = fetch_session_path(
            &directory,
            &roots,
            false,
            FetchMode::Content,
            tempfile::tempfile().unwrap(),
        )
        .unwrap();
        assert!(fetched.len() > 16 * 1024 * 1024);
        let mut wire = tempfile::tempfile().unwrap();
        fetched.write_to(&mut wire).unwrap();
        wire.rewind().unwrap();
        let truncated = wire.try_clone().unwrap();
        let decoded = FetchedPath::read_from(wire).unwrap();
        let mut first = decoded.reader();
        let mut second = decoded.reader();
        let mut prefix = [0; 128 * 1024];
        let mut other_prefix = [0; 128 * 1024];
        first.read_exact(&mut prefix).unwrap();
        second.read_exact(&mut other_prefix).unwrap();
        assert_eq!(
            prefix, other_prefix,
            "readers must not share the spool cursor"
        );
        let mut archive = zip::ZipArchive::new(decoded.reader()).unwrap();
        let mut restored = Vec::new();
        archive
            .by_name("probes/noise.bin")
            .unwrap()
            .read_to_end(&mut restored)
            .unwrap();
        assert_eq!(restored, noise);
        let bytes = std::io::copy(
            &mut archive.by_name("probes/zeros.bin").unwrap(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(bytes, 129 * 1024 * 1024);
        truncated
            .set_len(truncated.metadata().unwrap().len() - 1)
            .unwrap();
        assert!(
            FetchedPath::read_from(truncated)
                .unwrap_err()
                .to_string()
                .contains("incomplete fetch transfer")
        );
    }
}
