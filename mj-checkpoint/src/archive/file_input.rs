use super::{MAX_SESSION_FILE_BYTES, SessionExportError};
use anyhow::Context;
use mj_core::confined_path::{ConfinedDir, Publish, PublishError};
use std::io::Read;
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
    mj_core::config::validate_relative_destination(relative).map_err(refuse)?;
    if bytes.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(SessionExportError::Refused(format!(
            "file exceeds {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    let root = root.canonicalize().context("resolve session workspace")?;
    // The workspace's own processes can rename and relink its directories
    // while this runs, so every step resolves from handles opened without
    // following a link rather than from path text (#1419).
    let root = ConfinedDir::open_root(&root)?.context("session workspace disappeared")?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let name = relative
        .file_name()
        .context("file destination has no name")?;
    let directory = root.create_dir_all(parent).map_err(refuse)?;
    let publish = if overwrite {
        Publish::Replace
    } else {
        Publish::CreateNew
    };
    match directory.write_atomically(name, bytes, publish) {
        Ok(()) => Ok(()),
        Err(PublishError::NotARegularFile) => Err(SessionExportError::Refused(
            "destination is not a regular file".into(),
        )),
        Err(PublishError::AlreadyExists) => Err(SessionExportError::Refused(
            "file already exists; request overwrite explicitly".into(),
        )),
        Err(PublishError::Failed(error)) => Err(error.context("publish session file").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
