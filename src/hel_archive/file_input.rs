use super::{MAX_SESSION_FILE_BYTES, SessionExportError, ensure_no_symlink_ancestors};
use anyhow::Context;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

/// Bound input on both sides of the transport, including stdin with no length.
pub fn read_session_file_input(reader: impl Read) -> Result<Vec<u8>, SessionExportError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_SESSION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read incoming session file")?;
    if bytes.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(SessionExportError::Refused(format!(
            "file exceeds {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

/// Publish a complete file in an idle session's workspace. The caller owns the
/// worker barrier through completion, so queued harness work cannot race it.
pub fn write_session_file(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<(), SessionExportError> {
    let refuse = |error: anyhow::Error| SessionExportError::Refused(format!("{error:#}"));
    crate::hel_config::validate_relative_destination(relative).map_err(refuse)?;
    if bytes.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(SessionExportError::Refused(format!(
            "file exceeds {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    let root = root.canonicalize().context("resolve session workspace")?;
    ensure_no_symlink_ancestors(&root, relative).map_err(refuse)?;
    let destination = root.join(relative);
    let existing = match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(SessionExportError::Refused(
                    "destination is not a regular file".into(),
                ));
            }
            if !overwrite {
                return Err(SessionExportError::Refused(
                    "file already exists; request overwrite explicitly".into(),
                ));
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context("inspect file destination")
                .into());
        }
    };
    let parent = destination
        .parent()
        .context("file destination has no parent")?;
    fs::create_dir_all(parent).context("create session file parent directories")?;
    ensure_no_symlink_ancestors(&root, relative).map_err(refuse)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("stage session file")?;
    temporary
        .write_all(bytes)
        .context("write staged session file")?;
    if let Some(permissions) = existing {
        temporary
            .as_file()
            .set_permissions(permissions)
            .context("preserve file permissions")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("sync staged session file")?;
    ensure_no_symlink_ancestors(&root, relative).map_err(refuse)?;
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(SessionExportError::Refused(
                "destination is not a regular file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context("recheck file destination")
                .into());
        }
    }
    let result = if overwrite {
        temporary.persist(&destination)
    } else {
        temporary.persist_noclobber(&destination)
    };
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Err(
            SessionExportError::Refused("file already exists; request overwrite explicitly".into()),
        ),
        Err(error) => Err(anyhow::Error::new(error.error)
            .context("publish session file")
            .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_injection_is_binary_bounded_and_requires_explicit_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..131_337).map(|i| (i % 251) as u8).collect();
        let input = read_session_file_input(bytes.as_slice()).unwrap();
        let path = Path::new("nested/data.bin");
        write_session_file(root.path(), path, &input, false).unwrap();
        assert_eq!(fs::read(root.path().join(path)).unwrap(), bytes);
        assert!(write_session_file(root.path(), path, b"replacement", false).is_err());
        assert_eq!(fs::read(root.path().join(path)).unwrap(), bytes);
        write_session_file(root.path(), path, b"replacement", true).unwrap();
        assert_eq!(fs::read(root.path().join(path)).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(root.path().join("nested")).unwrap().count(), 1);
        for path in ["../outside", "/absolute", "nested/../../outside", ""] {
            assert!(
                write_session_file(root.path(), Path::new(path), b"bad", true).is_err(),
                "{path}"
            );
        }
        assert!(read_session_file_input(std::io::repeat(0)).is_err());
        assert!(
            write_session_file(
                root.path(),
                Path::new("too-big"),
                &vec![0; MAX_SESSION_FILE_BYTES as usize + 1],
                false
            )
            .is_err()
        );
        assert!(!root.path().join("too-big").exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_injection_refuses_symlinks_including_links_inside_the_workspace() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        symlink(root.path().join("real"), root.path().join("inside")).unwrap();
        fs::write(root.path().join("target"), b"original").unwrap();
        symlink(root.path().join("target"), root.path().join("leaf")).unwrap();
        for path in ["escape/new/file", "inside/new/file", "leaf"] {
            assert!(write_session_file(root.path(), Path::new(path), b"bad", true).is_err());
        }
        assert_eq!(fs::read(root.path().join("target")).unwrap(), b"original");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
