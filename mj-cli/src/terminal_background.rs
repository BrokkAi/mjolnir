//! Asks the terminal for its background color, for the Auto theme.
//!
//! The question is OSC 11 (`ESC ] 11 ; ? ESC \`), followed by Primary Device
//! Attributes (DA1, `ESC [ c`). Every VT-compatible terminal answers DA1 and
//! answers queries in order, so the DA1 reply marks the end of anything the
//! terminal will say about its background: a terminal that ignores OSC 11
//! costs one round trip, not a timeout.
//!
//! The replies must be read here, before crossterm's reader starts. crossterm
//! 0.29 parses an unknown `ESC ]` sequence as Alt and the characters after it,
//! so a reply that reached it would be typed into the composer.

use mj_chat::theme::Rgb;

/// The terminal's background, or `None` when it does not report one. Call in
/// raw mode, before anything else reads the terminal's input. Bytes that
/// arrive ahead of the replies, such as keys typed while mj starts, are
/// consumed with them.
pub(crate) fn query() -> Option<Rgb> {
    #[cfg(unix)]
    {
        match unix::ask() {
            Ok(Some(background)) => Some(background),
            Ok(None) => {
                tracing::debug!("the terminal did not report its background color");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "could not ask the terminal for its background color");
                None
            }
        }
    }
    // crossterm reads the Windows console as input records, not as the byte
    // stream a reply would arrive on.
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
    use std::time::{Duration, Instant};

    use super::Rgb;

    /// crossterm waits as long for its own keyboard query.
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);

    const QUERY: &[u8] = b"\x1b]11;?\x1b\\\x1b[c";

    pub(super) fn ask() -> io::Result<Option<Rgb>> {
        // The descriptor crossterm reads: standard input when it is the
        // terminal, otherwise the controlling terminal. crossterm writes its
        // own query to standard output, as the frames are.
        let stdin = io::stdin();
        let tty;
        let input = if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            stdin.as_fd()
        } else {
            tty = File::options().read(true).open("/dev/tty")?;
            tty.as_fd()
        };
        let mut stdout = io::stdout().lock();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
        read_replies(input, Instant::now() + ANSWER_TIMEOUT)
    }

    /// Reads one byte at a time, so nothing after the DA1 reply is taken
    /// from crossterm. A background that arrived is used even when DA1 never
    /// does.
    fn read_replies(input: BorrowedFd<'_>, deadline: Instant) -> io::Result<Option<Rgb>> {
        let mut replies = Vec::new();
        loop {
            if attributes_reply_end(&replies).is_some() {
                return Ok(background_reply(&replies));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(background_reply(&replies));
            }
            let mut poll = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let wait = i32::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
            match unsafe { libc::poll(&raw mut poll, 1, wait) } {
                0 => continue,
                ready if ready < 0 => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                _ => {}
            }
            let mut byte = 0_u8;
            match unsafe { libc::read(input.as_raw_fd(), (&raw mut byte).cast(), 1) } {
                1 => replies.push(byte),
                0 => return Ok(background_reply(&replies)),
                _ => {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    /// Where a DA1 reply (`ESC [ ? <digits and ;> c`) ends in `bytes`.
    fn attributes_reply_end(bytes: &[u8]) -> Option<usize> {
        let mut search = 0;
        while let Some(offset) = find(&bytes[search..], b"\x1b[?") {
            let start = search + offset + 3;
            let parameters = bytes[start..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
                .count();
            if bytes.get(start + parameters) == Some(&b'c') {
                return Some(start + parameters + 1);
            }
            search = start;
        }
        None
    }

    /// The color in an OSC 11 reply in `bytes`: `ESC ] 11 ; rgb:R/G/B` or
    /// `rgba:R/G/B/A`, each channel one to four hex digits, ended by BEL or
    /// `ESC \`.
    fn background_reply(bytes: &[u8]) -> Option<Rgb> {
        let start = find(bytes, b"\x1b]11;")? + 5;
        let body = &bytes[start..];
        let end = body
            .iter()
            .position(|byte| matches!(byte, b'\x07' | b'\x1b'))?;
        let spec = std::str::from_utf8(&body[..end]).ok()?;
        let channels = spec
            .strip_prefix("rgb:")
            .or_else(|| spec.strip_prefix("rgba:"))?;
        let mut channels = channels.split('/').map(channel);
        Some((channels.next()??, channels.next()??, channels.next()??))
    }

    /// A one-to-four-digit hex channel scaled to eight bits.
    fn channel(digits: &str) -> Option<u8> {
        if !(1..=4).contains(&digits.len()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        let max = (1_u32 << (4 * digits.len())) - 1;
        u8::try_from((value * 255 + max / 2) / max).ok()
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Replies as terminals send them: xterm and VTE with four digits and
        /// ST, kitty with BEL, rxvt-unicode with `rgba:`, mixed channel
        /// widths, a terminal that answers only DA1, and keys typed before
        /// the replies.
        #[test]
        fn background_replies_from_real_terminals_parse_up_to_the_attributes_reply() {
            let cases: [(&[u8], Option<Rgb>); 6] = [
                (
                    b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\\x1b[?65;1;9c",
                    Some((253, 246, 227)),
                ),
                (
                    b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07\x1b[?62;c",
                    Some((30, 30, 46)),
                ),
                (
                    b"\x1b]11;rgba:0000/2b2b/3636/ffff\x1b\\\x1b[?1;2c",
                    Some((0, 43, 54)),
                ),
                (b"\x1b]11;rgb:ff/80/0\x07\x1b[?6c", Some((255, 128, 0))),
                (b"\x1b[?1;2c", None),
                (
                    b"ls\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?64;1c",
                    Some((255, 255, 255)),
                ),
            ];
            for (reply, expected) in cases {
                assert_eq!(attributes_reply_end(reply), Some(reply.len()), "{reply:?}");
                assert_eq!(background_reply(reply), expected, "{reply:?}");
            }
            // A keyboard-flags reply is not DA1, and an unfinished DA1 reply
            // is still being read.
            assert_eq!(attributes_reply_end(b"\x1b[?1u"), None);
            assert_eq!(
                attributes_reply_end(b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?6"),
                None
            );
        }
    }
}
