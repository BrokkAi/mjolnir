//! Append-only storage for full transcript bodies kept outside SQLite.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use mj_core::transcript::TranscriptBody;

const MAGIC: &[u8; 4] = b"MJTD";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 15;

pub(super) fn data_dir_from(db_path: &Path) -> &Path {
    db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn detail_path(data_dir: &Path, session_id: &str) -> Result<PathBuf> {
    mj_core::config::validate_id("session", session_id)?;
    Ok(data_dir
        .join("tool-details")
        .join(format!("{session_id}.mjtd")))
}

/// Append one frame. Callers must serialize appends and deletes for a session.
pub fn append_tool_detail(
    data_dir: &Path,
    session_id: &str,
    stable_id: &str,
    body_json: &str,
) -> Result<(u64, u32)> {
    append_tool_details_batch(data_dir, session_id, [(stable_id, body_json)])
        .map(|frames| frames[0])
}

/// Persist a batch before publishing any of its pointers in SQLite.
/// The database writer serializes changes to each session file.
pub fn append_tool_details_batch<'a>(
    data_dir: &Path,
    session_id: &str,
    bodies: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<(u64, u32)>> {
    let path = detail_path(data_dir, session_id)?;
    let mut bodies = bodies.into_iter().peekable();
    if bodies.peek().is_none() {
        return Ok(Vec::new());
    }
    let directory = path.parent().context("missing tool detail directory")?;
    mj_core::config::create_owner_only_directory(directory)?;
    let mut options = OpenOptions::new();
    options.append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let (mut file, created) = match options.open(&path) {
        Ok(file) => (file, false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (
            options
                .create_new(true)
                .open(&path)
                .with_context(|| format!("create tool details {}", path.display()))?,
            true,
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("open tool details {}", path.display()));
        }
    };
    if created {
        mj_core::config::sync_directory(directory)?;
        mj_core::config::sync_directory(data_dir)?;
    }
    let mut offset = file.metadata()?.len();
    let mut frames = Vec::new();
    for (stable_id, body_json) in bodies {
        let id_len = u16::try_from(stable_id.len()).context("tool detail stable ID is too long")?;
        let payload =
            zstd::stream::encode_all(body_json.as_bytes(), 0).context("compress tool detail")?;
        let payload_len =
            u32::try_from(payload.len()).context("tool detail payload is too long")?;
        let len = u32::try_from(HEADER_LEN + stable_id.len() + payload.len())
            .context("tool detail frame is too long")?;
        let mut frame = Vec::with_capacity(len as usize);
        frame.extend(MAGIC);
        frame.push(VERSION);
        frame.extend(id_len.to_be_bytes());
        frame.extend(stable_id.as_bytes());
        frame.extend(crc32fast::hash(&payload).to_be_bytes());
        frame.extend(payload_len.to_be_bytes());
        frame.extend(payload);
        file.write_all(&frame)
            .with_context(|| format!("append tool detail {stable_id:?} to {}", path.display()))?;
        frames.push((offset, len));
        offset += u64::from(len);
    }
    file.sync_data()
        .with_context(|| format!("sync tool details {}", path.display()))?;
    Ok(frames)
}

pub fn read_tool_detail(
    data_dir: &Path,
    session_id: &str,
    expected_stable_id: &str,
    offset: u64,
    len: u32,
) -> Result<TranscriptBody> {
    let path = detail_path(data_dir, session_id)?;
    let file =
        File::open(&path).with_context(|| format!("open tool details {}", path.display()))?;
    ensure!(
        len as usize >= HEADER_LEN,
        "tool detail frame is shorter than its header"
    );
    let file_len = file
        .metadata()
        .with_context(|| format!("stat tool details {}", path.display()))?
        .len();
    ensure!(
        offset
            .checked_add(u64::from(len))
            .is_some_and(|end| end <= file_len),
        "short read of tool details {} at {offset}, expected {len} bytes",
        path.display()
    );
    let mut frame = vec![0; len as usize];
    read_exact_at(&file, &mut frame, offset).with_context(|| {
        format!(
            "short read of tool details {} at {offset}, expected {len} bytes",
            path.display()
        )
    })?;
    decode_frame(&frame, expected_stable_id)
        .with_context(|| format!("read tool detail in {} at {offset}", path.display()))
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected end of tool details file",
                ));
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn decode_frame(frame: &[u8], expected_stable_id: &str) -> Result<TranscriptBody> {
    ensure!(&frame[..4] == MAGIC, "bad tool detail magic");
    ensure!(
        frame[4] == VERSION,
        "unsupported tool detail version {}",
        frame[4]
    );
    let id_len = u16::from_be_bytes(frame[5..7].try_into()?) as usize;
    ensure!(
        frame.len() >= HEADER_LEN + id_len,
        "truncated tool detail header"
    );
    let id_end = 7 + id_len;
    let id =
        std::str::from_utf8(&frame[7..id_end]).context("tool detail stable ID is not UTF-8")?;
    ensure!(
        id == expected_stable_id,
        "tool detail stable ID mismatch: expected {expected_stable_id:?}, got {id:?}"
    );
    let checksum = u32::from_be_bytes(frame[id_end..id_end + 4].try_into()?);
    let payload_len = u32::from_be_bytes(frame[id_end + 4..id_end + 8].try_into()?);
    let payload = &frame[id_end + 8..];
    ensure!(
        payload.len() == payload_len as usize,
        "tool detail payload length mismatch"
    );
    ensure!(
        crc32fast::hash(payload) == checksum,
        "tool detail CRC32 mismatch"
    );
    let body = zstd::stream::decode_all(payload).context("decompress tool detail")?;
    serde_json::from_slice(&body).context("decode tool detail transcript body")
}

pub fn remove_session_tool_details(data_dir: &Path, session_id: &str) -> Result<()> {
    let path = detail_path(data_dir, session_id)?;
    match fs::remove_file(&path) {
        Ok(()) => {
            mj_core::config::sync_directory(path.parent().context("missing tool detail directory")?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove tool details {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_frames_reject_wrong_ids_corruption_and_truncation() {
        let data = tempfile::tempdir().unwrap();
        let mut seed = 0x1234_abcd_u32;
        let text: String = (0..192 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                char::from(b'!' + (seed % 90) as u8)
            })
            .collect();
        let body = TranscriptBody::System { text };
        let body = serde_json::to_string(&body).unwrap();
        let first = append_tool_detail(data.path(), "session", "first", &body).unwrap();
        let (offset, len) = append_tool_detail(data.path(), "session", "second", &body).unwrap();
        assert_eq!(offset, u64::from(first.1));
        assert!(len > 64 * 1024);
        let read = |id| read_tool_detail(data.path(), "session", id, offset, len);
        assert_eq!(
            serde_json::to_string(&read("second").unwrap()).unwrap(),
            body
        );
        assert!(format!("{:#}", read("first").unwrap_err()).contains("stable ID mismatch"));

        let path = detail_path(data.path(), "session").unwrap();
        let original = fs::read(&path).unwrap();
        for (index, expected) in [(0, "magic"), (4, "version"), (HEADER_LEN + 6, "CRC32")] {
            let mut corrupt = original.clone();
            corrupt[offset as usize + index] ^= 1;
            fs::write(&path, corrupt).unwrap();
            let error = format!("{:#}", read("second").unwrap_err());
            assert!(error.contains(expected), "{error}");
        }
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(format!("{:#}", read("second").unwrap_err()).contains("short read"));
        remove_session_tool_details(data.path(), "session").unwrap();
        let missing = read("second").unwrap_err();
        assert_eq!(
            missing.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert!(append_tool_detail(data.path(), "../session", "first", &body).is_err());
    }
}
