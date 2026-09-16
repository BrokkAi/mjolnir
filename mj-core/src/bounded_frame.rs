//! Newline-delimited frames read from a stream under a byte limit.
//!
//! Every JSON-lines peer (the relay proxy, the worker's socket, and the Codex
//! and Grok quota processes) reads frames the same way. What differs is what a
//! partial frame at end of stream means to that peer, so that decision stays
//! with the caller.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The outcome of reading one frame.
#[derive(Debug, PartialEq, Eq)]
pub enum BoundedFrame {
    /// A complete frame, without its newline.
    Line(Vec<u8>),
    /// The stream ended cleanly between frames.
    End,
    /// The stream ended part way through a frame; these are the bytes read.
    Truncated(Vec<u8>),
}

#[derive(Debug)]
pub enum BoundedFrameError {
    Io(std::io::Error),
    /// The frame grew past the limit before its newline arrived.
    TooLarge,
}

/// Read one frame of at most `maximum_bytes` bytes, excluding its newline.
/// The limit is enforced while reading, so an unterminated stream never
/// buffers more than the limit.
pub async fn read_bounded_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    maximum_bytes: usize,
) -> Result<BoundedFrame, BoundedFrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(BoundedFrameError::Io)?;
        if available.is_empty() {
            return Ok(if frame.is_empty() {
                BoundedFrame::End
            } else {
                BoundedFrame::Truncated(frame)
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content = newline.unwrap_or(available.len());
        if frame.len().saturating_add(content) > maximum_bytes {
            return Err(BoundedFrameError::TooLarge);
        }
        frame.extend_from_slice(&available[..content]);
        reader.consume(content + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(BoundedFrame::Line(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    /// Frames larger than a pipe buffer arrive in many reads; each must come
    /// back whole, the stream must end cleanly after the last one, and a frame
    /// one byte over the limit must be refused without buffering all of it.
    #[tokio::test]
    async fn frames_larger_than_a_pipe_buffer_are_read_whole_and_bounded() {
        const FRAME: usize = 200 * 1024;
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let producer = tokio::spawn(async move {
            for byte in [b'a', b'b'] {
                writer.write_all(&vec![byte; FRAME]).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
            }
            writer.write_all(&vec![b'c'; FRAME + 1]).await.unwrap();
        });
        let mut reader = BufReader::new(reader);

        for byte in [b'a', b'b'] {
            assert_eq!(
                read_bounded_frame(&mut reader, FRAME).await.unwrap(),
                BoundedFrame::Line(vec![byte; FRAME])
            );
        }
        assert!(matches!(
            read_bounded_frame(&mut reader, FRAME).await,
            Err(BoundedFrameError::TooLarge)
        ));
        producer.abort();
    }

    #[tokio::test]
    async fn end_of_stream_is_clean_between_frames_and_truncated_inside_one() {
        let mut reader = BufReader::new(&b"one\npartial"[..]);
        assert_eq!(
            read_bounded_frame(&mut reader, 64).await.unwrap(),
            BoundedFrame::Line(b"one".to_vec())
        );
        assert_eq!(
            read_bounded_frame(&mut reader, 64).await.unwrap(),
            BoundedFrame::Truncated(b"partial".to_vec())
        );
        assert_eq!(
            read_bounded_frame(&mut reader, 64).await.unwrap(),
            BoundedFrame::End
        );
    }
}
