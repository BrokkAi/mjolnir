use super::SessionExportError;
use anyhow::{Context, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPath {
    pub kind: FetchedKind,
    /// The path on the target after symlinks were resolved.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A fetched copy on the calling machine, without its content in the response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedFetch {
    pub path: PathBuf,
    pub kind: FetchedKind,
    pub source: String,
    pub bytes: usize,
}

#[derive(Serialize, Deserialize)]
struct FetchedHeader {
    kind: FetchedKind,
    path: String,
}

impl FetchedPath {
    /// Keep the source's whole path below `<temp>/mjolnir/<session>` so equal
    /// filenames from different paths or sessions cannot overwrite each other.
    pub fn local_path(&self, temp: &Path, session: &str) -> anyhow::Result<PathBuf> {
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
        for component in Path::new(&self.path).components() {
            if let std::path::Component::Normal(part) = component {
                local.push(part);
                named = true;
            }
        }
        ensure!(named, "the target answered no path for the fetch");
        if self.kind == FetchedKind::Directory {
            let mut name = local.file_name().unwrap_or_default().to_os_string();
            name.push(".zip");
            local.set_file_name(name);
        }
        Ok(local)
    }

    pub fn save(&self, temp: &Path, session: &str) -> anyhow::Result<SavedFetch> {
        let path = self.local_path(temp, session)?;
        mj_core::config::atomic_write(&path, &self.bytes)?;
        Ok(SavedFetch {
            path,
            kind: self.kind,
            source: self.path.clone(),
            bytes: self.bytes.len(),
        })
    }

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
    if mode == FetchMode::Listing {
        let bytes = list_path(&resolved, &metadata)?;
        return Ok(FetchedPath {
            kind: FetchedKind::Listing,
            path: display,
            bytes,
        });
    }
    if metadata.is_file() {
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
    let bytes = zip_directory(&resolved)?;
    Ok(FetchedPath {
        kind: FetchedKind::Directory,
        path: display,
        bytes,
    })
}

/// A [`SessionListing`] of `path` as JSON.
fn list_path(path: &Path, metadata: &fs::Metadata) -> Result<Vec<u8>, SessionExportError> {
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

fn zip_directory(directory: &Path) -> Result<Vec<u8>, SessionExportError> {
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
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
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
    let buffer = writer.finish().map_err(|error| failed(error, directory))?;
    Ok(buffer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn a_fetched_path_lands_under_its_session_and_cannot_leave_it() {
        let fetched_location =
            |temp: &Path, session: &str, fetched: &FetchedPath| fetched.local_path(temp, session);
        let temp = PathBuf::from("/scratch");
        let fetched = |kind, path: &str| FetchedPath {
            kind,
            path: path.to_owned(),
            bytes: Vec::new(),
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
        let fetched = fetch_session_path(&file, &roots, false, FetchMode::Content).unwrap();
        assert_eq!(
            FetchedPath::decode(fetched.encode().unwrap())
                .unwrap()
                .bytes,
            noise
        );
        // This compresses well but exceeds the former directory input limit.
        fs::File::create(directory.join("zeros.bin"))
            .unwrap()
            .set_len(129 * 1024 * 1024)
            .unwrap();
        let fetched = fetch_session_path(&directory, &roots, false, FetchMode::Content).unwrap();
        assert!(fetched.bytes.len() > 16 * 1024 * 1024);
        let decoded = FetchedPath::decode(fetched.encode().unwrap()).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(decoded.bytes)).unwrap();
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
    }
}
