use super::SessionExportError;
use anyhow::Context;
use mj_core::confined_path::{ConfinedDir, Publish, PublishError};
use std::io::Read;
use std::path::Path;

/// Publish a complete file in an idle session's workspace. The caller owns the
/// worker barrier through completion, so queued harness work cannot race it.
pub fn write_session_file(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<(), SessionExportError> {
    let mut reader = bytes;
    write_session_file_from_reader(
        root,
        relative,
        &mut reader,
        Some(bytes.len() as u64),
        overwrite,
    )?;
    Ok(())
}

/// Stream a file from `reader` and atomically publish it in an idle session's
/// workspace. When `expected_len` is `Some`, rejects short reads and trailing
/// extra bytes without publishing.
pub fn write_session_file_from_reader(
    root: &Path,
    relative: &Path,
    reader: &mut dyn Read,
    expected_len: Option<u64>,
    overwrite: bool,
) -> Result<u64, SessionExportError> {
    let refuse = |error: anyhow::Error| SessionExportError::Refused(format!("{error:#}"));
    mj_core::config::validate_relative_destination(relative).map_err(refuse)?;
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
    match directory.write_atomically_from_reader(name, reader, expected_len, publish) {
        Ok(written) => Ok(written),
        Err(PublishError::NotARegularFile) => Err(SessionExportError::Refused(
            "destination is not a regular file".into(),
        )),
        Err(PublishError::AlreadyExists) => Err(SessionExportError::Refused(
            "file already exists; request overwrite explicitly".into(),
        )),
        Err(PublishError::LengthMismatch { expected, received }) => {
            Err(SessionExportError::Refused(format!(
                "incomplete file upload: expected {expected} bytes, received {received}"
            )))
        }
        Err(PublishError::Failed(error)) => Err(error.context("publish session file").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;

    #[test]
    fn file_injection_is_binary_streamed_and_requires_explicit_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..131_337).map(|i| (i % 251) as u8).collect();
        let path = Path::new("nested/data.bin");
        write_session_file(root.path(), path, &bytes, false).unwrap();
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
        // Files over 16 MiB stream without an artificial cap.
        let large_len = super::super::MAX_SESSION_FILE_BYTES + 65_536;
        let mut large_reader = io::repeat(0x5a).take(large_len);
        let written = write_session_file_from_reader(
            root.path(),
            Path::new("large.bin"),
            &mut large_reader,
            Some(large_len),
            false,
        )
        .unwrap();
        assert_eq!(written, large_len);
        assert_eq!(
            fs::metadata(root.path().join("large.bin")).unwrap().len(),
            large_len
        );
    }

    #[test]
    fn streaming_write_rejects_length_mismatch_and_read_error_without_publishing() {
        struct FailingReader {
            remaining: usize,
        }
        impl Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "simulated mid-stream disconnect",
                    ));
                }
                let n = buf.len().min(self.remaining);
                buf[..n].fill(0xab);
                self.remaining -= n;
                Ok(n)
            }
        }

        let root = tempfile::tempdir().unwrap();
        // Short read against declared length.
        let mut short = &b"hello"[..];
        assert!(
            write_session_file_from_reader(
                root.path(),
                Path::new("short.bin"),
                &mut short,
                Some(10),
                false,
            )
            .is_err()
        );
        assert!(!root.path().join("short.bin").exists());

        // Trailing extra bytes against declared length.
        let mut extra = &b"hello world"[..];
        assert!(
            write_session_file_from_reader(
                root.path(),
                Path::new("extra.bin"),
                &mut extra,
                Some(5),
                false,
            )
            .is_err()
        );
        assert!(!root.path().join("extra.bin").exists());

        // Mid-stream read error with unknown length (chunked).
        let mut failing = FailingReader {
            remaining: 128 * 1024,
        };
        assert!(
            write_session_file_from_reader(
                root.path(),
                Path::new("failed.bin"),
                &mut failing,
                None,
                false,
            )
            .is_err()
        );
        assert!(!root.path().join("failed.bin").exists());
        // No leftover temp files in root.
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
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
