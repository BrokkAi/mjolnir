//! Clipboard copy backend for Ctrl-Y and Ctrl-Shift-C.
//!
//! This module decides *how* to get text onto the user's clipboard based on the
//! current environment. The selection order is:
//!
//! 1. **SSH session** (`SSH_TTY` / `SSH_CONNECTION` set): use tmux clipboard
//!    integration when available, otherwise OSC 52, because the native clipboard
//!    belongs to the remote machine.
//! 2. **Local session**: try `arboard` (native clipboard) first. On WSL, fall back
//!    to the Windows clipboard through PowerShell if `arboard` fails. Finally, fall
//!    back to terminal-mediated copy if no native/WSL clipboard path succeeds.
//!
//! On Linux, X11 and some Wayland compositors require the process that wrote the
//! clipboard to keep its handle open. `ClipboardLease` wraps the `arboard::Clipboard`
//! so callers can store it for the lifetime of the TUI. On other platforms the lease
//! is always `None`.
//!
//! The module is intentionally narrow: text copy only, user-facing error strings,
//! no reusable clipboard abstraction.

use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

/// Maximum raw bytes we will base64-encode into an OSC 52 sequence.
/// Large payloads are rejected before encoding to avoid overwhelming the terminal.
const OSC52_MAX_RAW_BYTES: usize = 100_000;
/// Maximum image file size accepted for prompt attachments.
const PROMPT_IMAGE_MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Maximum decoded image pixel count accepted for prompt attachments.
const PROMPT_IMAGE_MAX_PIXELS: u64 = 16_000_000;
/// Maximum encoded PNG size accepted before base64 conversion.
const PROMPT_IMAGE_MAX_PNG_BYTES: usize = 25 * 1024 * 1024;

/// PNG image data read from the system clipboard and prepared for ACP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
}

/// Read image content from the system clipboard and encode it as PNG.
pub fn read_clipboard_image_as_png() -> Result<ClipboardImage, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;

    if let Ok(files) = clipboard.get().file_list()
        && let Some(image) = files
            .into_iter()
            .find_map(|path| load_image_path_as_png(&path).ok())
    {
        return Ok(image);
    }

    let image = clipboard
        .get_image()
        .map_err(|e| format!("no image on clipboard: {e}"))?;
    let width = image.width as u32;
    let height = image.height as u32;
    validate_prompt_image_dimensions(width, height, "clipboard image")?;
    let rgba = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
        .ok_or_else(|| "could not encode image: invalid RGBA buffer".to_string())?;

    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("could not encode image: {e}"))?;

    clipboard_image_from_png(png, width, height, "clipboard image")
}

/// Read an image file and encode it as PNG for ACP prompt submission.
pub fn load_image_path_as_png(path: &Path) -> Result<ClipboardImage, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("could not read image metadata {}: {e}", path.display()))?;
    if metadata.len() > PROMPT_IMAGE_MAX_FILE_BYTES {
        return Err(format!(
            "image file {} is too large ({} bytes; max {PROMPT_IMAGE_MAX_FILE_BYTES})",
            path.display(),
            metadata.len()
        ));
    }

    let reader = image::ImageReader::open(path)
        .map_err(|e| format!("could not open image {}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("could not identify image {}: {e}", path.display()))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| format!("could not inspect image {}: {e}", path.display()))?;
    validate_prompt_image_dimensions(width, height, &path.display().to_string())?;

    let image =
        image::open(path).map_err(|e| format!("could not open image {}: {e}", path.display()))?;

    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("could not encode image {}: {e}", path.display()))?;

    clipboard_image_from_png(png, width, height, &path.display().to_string())
}

fn validate_prompt_image_dimensions(width: u32, height: u32, source: &str) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "image {source} has invalid dimensions {width}x{height}"
        ));
    }

    let pixels = u64::from(width) * u64::from(height);
    if pixels > PROMPT_IMAGE_MAX_PIXELS {
        return Err(format!(
            "image {source} is too large ({width}x{height}; max {PROMPT_IMAGE_MAX_PIXELS} pixels)"
        ));
    }
    Ok(())
}

fn clipboard_image_from_png(
    png: Vec<u8>,
    width: u32,
    height: u32,
    source: &str,
) -> Result<ClipboardImage, String> {
    if png.len() > PROMPT_IMAGE_MAX_PNG_BYTES {
        return Err(format!(
            "encoded image {source} is too large ({} bytes; max {PROMPT_IMAGE_MAX_PNG_BYTES})",
            png.len()
        ));
    }

    Ok(ClipboardImage {
        data_base64: BASE64_STANDARD.encode(&png),
        mime_type: "image/png".to_string(),
        width,
        height,
        byte_len: png.len(),
    })
}

/// Copy text to the system clipboard.
///
/// Over SSH, uses terminal-mediated copy so the text reaches the *local*
/// terminal emulator's clipboard rather than a remote X11/Wayland clipboard
/// that the user cannot access. On a local session, tries `arboard` (native
/// clipboard) first and falls back to WSL PowerShell, then terminal-mediated
/// copy, if needed.
///
/// OSC 52 is supported by kitty, WezTerm, iTerm2, Ghostty, and others.
pub fn copy_to_clipboard(text: &str) -> Result<Option<ClipboardLease>, String> {
    copy_to_clipboard_with(
        text,
        CopyEnvironment {
            ssh_session: is_ssh_session(),
            wsl_session: is_wsl_session(),
            tmux_session: is_tmux_session(),
        },
        tmux_clipboard_copy,
        osc52_copy,
        arboard_copy,
        wsl_clipboard_copy,
    )
}

impl std::fmt::Debug for ClipboardLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardLease").finish_non_exhaustive()
    }
}

/// Keeps a platform clipboard owner alive when the backend requires one.
///
/// On Linux/X11 and some Wayland compositors, clipboard contents are served by the
/// owning process. Dropping the `arboard::Clipboard` before the user pastes causes
/// the content to vanish. Store this lease on the widget that triggered the copy so
/// the handle lives as long as the TUI does. On non-Linux native paths and OSC 52
/// paths the lease is `None` — those backends do not require process-lifetime
/// ownership.
pub struct ClipboardLease {
    #[cfg(target_os = "linux")]
    _clipboard: Option<arboard::Clipboard>,
}

impl ClipboardLease {
    #[cfg(target_os = "linux")]
    fn native_linux(clipboard: arboard::Clipboard) -> Self {
        Self {
            _clipboard: Some(clipboard),
        }
    }

    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            _clipboard: None,
        }
    }
}

/// Core copy logic with injected backends, enabling deterministic unit tests
/// without touching real clipboards or terminal I/O.
#[derive(Clone, Copy)]
struct CopyEnvironment {
    ssh_session: bool,
    wsl_session: bool,
    tmux_session: bool,
}

fn copy_to_clipboard_with(
    text: &str,
    environment: CopyEnvironment,
    tmux_copy_fn: impl Fn(&str) -> Result<(), String>,
    osc52_copy_fn: impl Fn(&str) -> Result<(), String>,
    arboard_copy_fn: impl Fn(&str) -> Result<Option<ClipboardLease>, String>,
    wsl_copy_fn: impl Fn(&str) -> Result<(), String>,
) -> Result<Option<ClipboardLease>, String> {
    if environment.ssh_session {
        // Over SSH the native clipboard writes to the remote machine which is
        // useless. Terminal-mediated copy reaches the local terminal emulator.
        return terminal_clipboard_copy_with(
            text,
            environment.tmux_session,
            &tmux_copy_fn,
            &osc52_copy_fn,
        )
        .map(|()| None)
        .map_err(|terminal_err| {
            tracing::warn!("terminal clipboard copy failed over SSH: {terminal_err}");
            if environment.tmux_session {
                format!("terminal clipboard copy failed over SSH: {terminal_err}")
            } else {
                format!("OSC 52 clipboard copy failed over SSH: {terminal_err}")
            }
        });
    }

    match arboard_copy_fn(text) {
        Ok(lease) => Ok(lease),
        Err(native_err) => {
            if environment.wsl_session {
                tracing::warn!(
                    "native clipboard copy failed: {native_err}, falling back to WSL PowerShell"
                );
                match wsl_copy_fn(text) {
                    Ok(()) => return Ok(None),
                    Err(wsl_err) => {
                        tracing::warn!(
                            "WSL PowerShell clipboard copy failed: {wsl_err}, falling back to terminal clipboard"
                        );
                        return terminal_clipboard_copy_with(
                            text,
                            environment.tmux_session,
                            &tmux_copy_fn,
                            &osc52_copy_fn,
                        )
                        .map(|()| None)
                        .map_err(|terminal_err| {
                            if environment.tmux_session {
                                format!(
                                    "native clipboard: {native_err}; WSL fallback: {wsl_err}; terminal fallback: {terminal_err}"
                                )
                            } else {
                                format!(
                                    "native clipboard: {native_err}; WSL fallback: {wsl_err}; OSC 52 fallback: {terminal_err}"
                                )
                            }
                        });
                    }
                }
            }
            tracing::warn!(
                "native clipboard copy failed: {native_err}, falling back to terminal clipboard"
            );
            terminal_clipboard_copy_with(
                text,
                environment.tmux_session,
                &tmux_copy_fn,
                &osc52_copy_fn,
            )
            .map(|()| None)
            .map_err(|terminal_err| {
                if environment.tmux_session {
                    format!("native clipboard: {native_err}; terminal fallback: {terminal_err}")
                } else {
                    format!("native clipboard: {native_err}; OSC 52 fallback: {terminal_err}")
                }
            })
        }
    }
}

/// Copy through the active terminal, preferring tmux's native clipboard path.
fn terminal_clipboard_copy_with(
    text: &str,
    tmux_session: bool,
    tmux_copy_fn: &impl Fn(&str) -> Result<(), String>,
    osc52_copy_fn: &impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    if tmux_session {
        match tmux_copy_fn(text) {
            Ok(()) => return Ok(()),
            Err(tmux_err) => {
                tracing::warn!("tmux clipboard copy failed: {tmux_err}, falling back to OSC 52");
                return osc52_copy_fn(text).map_err(|osc_err| {
                    format!("tmux clipboard: {tmux_err}; OSC 52 fallback: {osc_err}")
                });
            }
        }
    }

    osc52_copy_fn(text)
}

/// Detect whether the current process is running inside an SSH session.
fn is_ssh_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

/// Detect whether the current process is running inside tmux.
fn is_tmux_session() -> bool {
    std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some()
}

#[cfg(target_os = "linux")]
fn is_wsl_session() -> bool {
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        version.to_lowercase().contains("microsoft") || version.to_lowercase().contains("wsl")
    } else {
        false
    }
}

#[cfg(not(target_os = "linux"))]
fn is_wsl_session() -> bool {
    false
}

/// Run arboard with stderr suppressed.
///
/// On macOS, `arboard::Clipboard::new()` initializes `NSPasteboard` which
/// triggers `os_log` / `NSLog` output on stderr. Because the TUI owns the
/// terminal, that stray output corrupts the display. We temporarily redirect
/// fd 2 to `/dev/null` around the call to keep the screen clean.
#[cfg(target_os = "macos")]
fn arboard_copy(text: &str) -> Result<Option<ClipboardLease>, String> {
    let _guard = SuppressStderr::new();
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("failed to set clipboard text: {e}"))?;
    Ok(None)
}

/// Run arboard with stderr suppressed.
///
/// On Linux/X11 and some Wayland setups, clipboard contents are served by the
/// process that last wrote them. Keep the `Clipboard` alive so the copied text
/// remains pasteable while the TUI is running.
#[cfg(target_os = "linux")]
fn arboard_copy(text: &str) -> Result<Option<ClipboardLease>, String> {
    let _guard = SuppressStderr::new();
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("failed to set clipboard text: {e}"))?;
    Ok(Some(ClipboardLease::native_linux(clipboard)))
}

/// arboard on Windows/other platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn arboard_copy(text: &str) -> Result<Option<ClipboardLease>, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("failed to set clipboard text: {e}"))?;
    Ok(None)
}

/// Copy text into the Windows clipboard from a WSL process.
#[cfg(target_os = "linux")]
fn wsl_clipboard_copy(text: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("powershell.exe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .args([
            "-NoProfile",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; $ErrorActionPreference = 'Stop'; $text = [Console]::In.ReadToEnd(); Set-Clipboard -Value $text",
        ])
        .spawn()
        .map_err(|e| format!("failed to spawn powershell.exe: {e}"))?;

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to open powershell.exe stdin".to_string());
    };

    if let Err(err) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("failed to write to powershell.exe: {err}"));
    }

    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for powershell.exe: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            let status = output.status;
            Err(format!("powershell.exe exited with status {status}"))
        } else {
            Err(format!("powershell.exe failed: {stderr}"))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn wsl_clipboard_copy(_text: &str) -> Result<(), String> {
    Err("WSL clipboard fallback unavailable on this platform".to_string())
}

/// Copy text through tmux's native clipboard integration.
///
/// `load-buffer -w -` lets tmux read the text from stdin, keep a matching tmux
/// paste buffer, and forward the contents to the outer terminal clipboard when
/// possible without relying on DCS passthrough.
fn tmux_clipboard_copy(text: &str) -> Result<(), String> {
    tmux_clipboard_copy_ready(
        || tmux_command_output(["show-options", "-gv", "set-clipboard"]),
        || tmux_command_output(["info"]),
    )?;

    let mut child = std::process::Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to open tmux stdin".to_string());
    };

    if let Err(err) = stdin.write_all(text.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("failed to write to tmux: {err}"));
    }

    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for tmux: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            let status = output.status;
            Err(format!("tmux exited with status {status}"))
        } else {
            Err(format!("tmux failed: {stderr}"))
        }
    }
}

/// Verify that tmux is configured to forward clipboard writes to the outer terminal.
fn tmux_clipboard_copy_ready(
    set_clipboard_fn: impl FnOnce() -> Result<String, String>,
    tmux_info_fn: impl FnOnce() -> Result<String, String>,
) -> Result<(), String> {
    let set_clipboard = set_clipboard_fn()?;
    if set_clipboard.trim() == "off" {
        return Err("tmux clipboard forwarding is disabled".to_string());
    }

    let tmux_info = tmux_info_fn()?;
    if tmux_info.lines().any(|line| line.contains("Ms: [missing]")) {
        return Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string());
    }

    Ok(())
}

fn tmux_command_output<const N: usize>(args: [&str; N]) -> Result<String, String> {
    let output = std::process::Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn tmux: {e}"))?;

    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|e| format!("tmux output was not UTF-8: {e}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            let status = output.status;
            Err(format!("tmux exited with status {status}"))
        } else {
            Err(format!("tmux failed: {stderr}"))
        }
    }
}

/// RAII guard that redirects stderr (fd 2) to `/dev/null` on creation and
/// restores the original fd on drop. Keeps arboard's NSPasteboard logs off
/// the TUI screen.
#[cfg(target_os = "macos")]
struct SuppressStderr {
    saved_fd: Option<libc::c_int>,
}

#[cfg(target_os = "macos")]
impl SuppressStderr {
    fn new() -> Self {
        unsafe {
            // Save the current stderr fd.
            let saved = libc::dup(2);
            if saved < 0 {
                return Self { saved_fd: None };
            }
            // Open /dev/null and point fd 2 at it.
            let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if devnull < 0 {
                libc::close(saved);
                return Self { saved_fd: None };
            }
            if libc::dup2(devnull, 2) < 0 {
                libc::close(saved);
                libc::close(devnull);
                return Self { saved_fd: None };
            }
            libc::close(devnull);
            Self {
                saved_fd: Some(saved),
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SuppressStderr {
    fn drop(&mut self) {
        if let Some(saved) = self.saved_fd {
            unsafe {
                libc::dup2(saved, 2);
                libc::close(saved);
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct SuppressStderr;

#[cfg(target_os = "linux")]
impl SuppressStderr {
    fn new() -> Self {
        Self
    }
}

/// Write text to the clipboard via the OSC 52 terminal escape sequence.
///
/// Prefers writing directly to `/dev/tty` (the controlling terminal) when
/// available, which avoids issues when stdout is piped or captured. Falls back
/// to stdout otherwise.
fn osc52_copy(text: &str) -> Result<(), String> {
    let tmux = std::env::var_os("TMUX").is_some();
    let sequence = osc52_sequence(text, tmux)?;

    #[cfg(unix)]
    {
        match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            Ok(tty) => match write_osc52_to_writer(tty, &sequence) {
                Ok(()) => return Ok(()),
                Err(err) => tracing::debug!(
                    "failed to write OSC 52 to /dev/tty: {err}; falling back to stdout"
                ),
            },
            Err(err) => {
                tracing::debug!("failed to open /dev/tty for OSC 52: {err}; falling back to stdout")
            }
        }
    }

    write_osc52_to_writer(std::io::stdout().lock(), &sequence)
}

fn write_osc52_to_writer(mut writer: impl Write, sequence: &str) -> Result<(), String> {
    writer
        .write_all(sequence.as_bytes())
        .map_err(|e| format!("failed to write OSC 52: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("failed to flush OSC 52: {e}"))
}

fn osc52_sequence(text: &str, tmux: bool) -> Result<String, String> {
    let raw_bytes = text.len();
    if raw_bytes > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "OSC 52 payload too large ({raw_bytes} bytes; max {OSC52_MAX_RAW_BYTES})"
        ));
    }

    let encoded = base64_encode(text.as_bytes());
    if tmux {
        Ok(format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\"))
    } else {
        Ok(format!("\x1b]52;c;{encoded}\x07"))
    }
}

/// Base64 encode without external dependencies.
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };

        result.push(CHARS[b0 >> 2] as char);
        result.push(CHARS[((b0 & 0x03) << 4) | (b1 >> 4)] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn remote_environment() -> CopyEnvironment {
        CopyEnvironment {
            ssh_session: true,
            wsl_session: true,
            tmux_session: false,
        }
    }

    fn remote_tmux_environment() -> CopyEnvironment {
        CopyEnvironment {
            tmux_session: true,
            ..remote_environment()
        }
    }

    fn local_environment() -> CopyEnvironment {
        CopyEnvironment {
            ssh_session: false,
            wsl_session: false,
            tmux_session: false,
        }
    }

    fn local_wsl_environment() -> CopyEnvironment {
        CopyEnvironment {
            wsl_session: true,
            ..local_environment()
        }
    }

    fn local_tmux_environment() -> CopyEnvironment {
        CopyEnvironment {
            tmux_session: true,
            ..local_environment()
        }
    }

    #[test]
    fn base64_encode_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encode_single_byte() {
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_encode_two_bytes() {
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn base64_encode_three_bytes() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_encode_hello_world() {
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
    }

    #[test]
    fn osc52_rejects_payload_larger_than_limit() {
        let text = "x".repeat(OSC52_MAX_RAW_BYTES + 1);
        assert_eq!(
            osc52_sequence(&text, false),
            Err(format!(
                "OSC 52 payload too large ({} bytes; max {OSC52_MAX_RAW_BYTES})",
                OSC52_MAX_RAW_BYTES + 1
            ))
        );
    }

    #[test]
    fn prompt_image_dimensions_reject_oversized_images() {
        let too_many_pixels = PROMPT_IMAGE_MAX_PIXELS + 1;
        assert_eq!(
            validate_prompt_image_dimensions(too_many_pixels as u32, 1, "test image"),
            Err(format!(
                "image test image is too large ({too_many_pixels}x1; max {PROMPT_IMAGE_MAX_PIXELS} pixels)"
            ))
        );
    }

    #[test]
    fn load_image_path_rejects_large_file_before_decode() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("too-large.png");
        let file = std::fs::File::create(&path).expect("create file");
        file.set_len(PROMPT_IMAGE_MAX_FILE_BYTES + 1)
            .expect("set sparse file length");

        let err = load_image_path_as_png(&path).expect_err("large file should be rejected");

        assert!(err.contains("is too large"), "unexpected error: {err}");
    }

    #[test]
    fn osc52_wraps_tmux_passthrough() {
        assert_eq!(
            osc52_sequence("hello", true),
            Ok("\u{1b}Ptmux;\u{1b}\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}\\".to_string())
        );
    }

    #[test]
    fn write_osc52_to_writer_emits_sequence_verbatim() {
        let sequence = "\u{1b}]52;c;aGVsbG8=\u{7}";
        let mut output = Vec::new();
        assert_eq!(write_osc52_to_writer(&mut output, sequence), Ok(()));
        assert_eq!(output, sequence.as_bytes());
    }

    #[test]
    fn ssh_uses_osc52_and_skips_native_on_success() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(tmux_calls.get(), 0);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn ssh_returns_osc52_error_and_skips_native() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Err("blocked".into())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        let Err(error) = result else {
            panic!("expected OSC 52 error");
        };
        assert_eq!(error, "OSC 52 clipboard copy failed over SSH: blocked");
        assert_eq!(tmux_calls.get(), 0);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_prefers_tmux_clipboard() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 0);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_falls_back_to_osc52_when_tmux_copy_fails() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Err("tmux unavailable".into())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(None)
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 0);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn ssh_inside_tmux_reports_tmux_and_osc52_errors_when_both_fail() {
        let result = copy_to_clipboard_with(
            "hello",
            remote_tmux_environment(),
            |_| Err("tmux unavailable".into()),
            |_| Err("osc blocked".into()),
            |_| Ok(None),
            |_| Ok(()),
        );

        let Err(error) = result else {
            panic!("expected tmux and OSC 52 errors");
        };
        assert_eq!(
            error,
            "terminal clipboard copy failed over SSH: tmux clipboard: tmux unavailable; OSC 52 fallback: osc blocked"
        );
    }

    #[test]
    fn tmux_clipboard_copy_ready_accepts_forwarding_configuration() {
        let result = tmux_clipboard_copy_ready(
            || Ok("external\n".to_string()),
            || Ok("193: Ms: (string) \\033]52;%p1%s;%p2%s\\a\n".to_string()),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn tmux_clipboard_copy_ready_rejects_disabled_forwarding() {
        let result = tmux_clipboard_copy_ready(
            || Ok("off\n".to_string()),
            || panic!("tmux info should not be queried when forwarding is disabled"),
        );

        assert_eq!(
            result,
            Err("tmux clipboard forwarding is disabled".to_string())
        );
    }

    #[test]
    fn tmux_clipboard_copy_ready_rejects_missing_ms_capability() {
        let result = tmux_clipboard_copy_ready(
            || Ok("external\n".to_string()),
            || Ok("193: Ms: [missing]\n".to_string()),
        );

        assert_eq!(
            result,
            Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string())
        );
    }

    #[test]
    fn local_uses_native_clipboard_first() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_wsl_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Ok(Some(ClipboardLease::test()))
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(Some(_))));
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn local_non_wsl_falls_back_to_osc52_when_native_fails() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn local_tmux_fallback_prefers_tmux_when_native_fails() {
        let tmux_calls = Cell::new(0_u8);
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_tmux_environment(),
            |_| {
                tmux_calls.set(tmux_calls.get() + 1);
                Ok(())
            },
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(tmux_calls.get(), 1);
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn local_wsl_native_failure_uses_powershell_and_skips_osc52_on_success() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_wsl_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(osc_calls.get(), 0);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 1);
    }

    #[test]
    fn local_wsl_falls_back_to_osc52_when_native_and_powershell_fail() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_wsl_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Ok(())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Err("powershell unavailable".into())
            },
        );

        assert!(matches!(result, Ok(None)));
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 1);
    }

    #[test]
    fn local_reports_both_errors_when_native_and_osc52_fail() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Err("osc blocked".into())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Ok(())
            },
        );

        let Err(error) = result else {
            panic!("expected native and OSC 52 errors");
        };
        assert_eq!(
            error,
            "native clipboard: native unavailable; OSC 52 fallback: osc blocked"
        );
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 0);
    }

    #[test]
    fn local_wsl_reports_native_powershell_and_osc52_errors_when_all_fail() {
        let osc_calls = Cell::new(0_u8);
        let native_calls = Cell::new(0_u8);
        let wsl_calls = Cell::new(0_u8);
        let result = copy_to_clipboard_with(
            "hello",
            local_wsl_environment(),
            |_| Ok(()),
            |_| {
                osc_calls.set(osc_calls.get() + 1);
                Err("osc blocked".into())
            },
            |_| {
                native_calls.set(native_calls.get() + 1);
                Err("native unavailable".into())
            },
            |_| {
                wsl_calls.set(wsl_calls.get() + 1);
                Err("powershell unavailable".into())
            },
        );

        let Err(error) = result else {
            panic!("expected native, WSL, and OSC 52 errors");
        };
        assert_eq!(
            error,
            "native clipboard: native unavailable; WSL fallback: powershell unavailable; OSC 52 fallback: osc blocked"
        );
        assert_eq!(osc_calls.get(), 1);
        assert_eq!(native_calls.get(), 1);
        assert_eq!(wsl_calls.get(), 1);
    }
}
