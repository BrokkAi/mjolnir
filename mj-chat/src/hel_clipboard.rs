//! Small cross-platform clipboard boundary shared by terminal text editors.
//!
//! Clipboard access is deliberately kept behind this module. Opening a native
//! clipboard can block, and WSL does not expose the Windows clipboard through
//! the Linux clipboard libraries that [`arboard`] normally uses.

#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Mutex;
#[cfg(any(target_os = "linux", all(test, unix)))]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
#[cfg(any(target_os = "linux", all(test, unix)))]
use hel::hel_targets::CommandExecutor;
use serde::{Deserialize, Serialize};

/// Keep one image comfortably below the one-megabyte durable relay command
/// budget after PNG base64 encoding and JSON framing are added.
pub const MAX_IMAGE_BYTES: usize = 700 * 1024;
const MAX_IMAGE_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;
#[cfg(target_os = "linux")]
const WSL_CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const WSL_CLIPBOARD_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
for ($attempt = 0; $attempt -lt 3; $attempt++) {
    try {
        $kind = $null
        $payload = $null
        if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
            $image = [System.Windows.Forms.Clipboard]::GetImage()
            if ($null -eq $image) { throw 'clipboard image disappeared while reading' }
            $stream = [System.IO.MemoryStream]::new()
            try {
                $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
                if ($stream.Length -gt $maxImageBytes) { throw "Image is too large; use a smaller screenshot (700 KiB maximum)" }
                $kind = 'IMAGE'
                $payload = [Convert]::ToBase64String($stream.ToArray())
            } finally {
                $stream.Dispose()
                $image.Dispose()
            }
        } elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {
            $kind = 'TEXT'
            $payload = [System.Windows.Forms.Clipboard]::GetText()
        } else {
            $kind = 'EMPTY'
            $payload = ''
        }
        [Console]::Out.Write("$kind`n")
        [Console]::Out.Write($payload)
        break
    } catch [System.Runtime.InteropServices.ExternalException] {
        if ($attempt -eq 2) { throw }
        Start-Sleep -Milliseconds 50
    }
}
"#;
#[cfg(target_os = "linux")]
const WSL_TEXT_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
for ($attempt = 0; $attempt -lt 3; $attempt++) {
    try {
        if ([System.Windows.Forms.Clipboard]::ContainsText()) {
            $kind = 'TEXT'
            $payload = [System.Windows.Forms.Clipboard]::GetText()
        } else {
            $kind = 'EMPTY'
            $payload = ''
        }
        [Console]::Out.Write("$kind`n")
        [Console]::Out.Write($payload)
        break
    } catch [System.Runtime.InteropServices.ExternalException] {
        if ($attempt -eq 2) { throw }
        Start-Sleep -Milliseconds 50
    }
}
"#;

/// A clipboard image is embedded in an ACP prompt, so it carries its bytes
/// instead of a host path that a worker container could not access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardImage {
    /// Base64-encoded PNG bytes.
    pub data_base64: String,
    pub mime_type: String,
}

impl ClipboardImage {
    /// Construct and validate an image from encoded PNG bytes.
    pub fn from_png_base64(data_base64: String) -> Result<Self> {
        Self::from_base64(data_base64, "image/png".to_owned())
    }

    /// Construct an embedded image from a serialized ACP image block. PNG
    /// headers are bounded; other image media types are retained when their base64
    /// bytes are valid so queued prompts from the web surface are not silently
    /// downgraded to text during terminal editing.
    pub fn from_base64(data_base64: String, mime_type: String) -> Result<Self> {
        let bytes = decode_base64(&data_base64)?;
        if mime_type.eq_ignore_ascii_case("image/png") {
            validate_png(&bytes)?;
        } else if !mime_type.to_ascii_lowercase().starts_with("image/") {
            bail!("clipboard image has unsupported media type {mime_type}");
        }
        Ok(Self {
            data_base64,
            mime_type,
        })
    }
}

/// The clipboard value selected for the caller. An image wins when both
/// representations are available; callers that edit ordinary text fields can
/// explicitly accept only [`Self::Text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image(ClipboardImage),
}

// Linux clipboards are owned by the process that last supplied their contents.
// Keeping one handle alive prevents copied text from disappearing and avoids
// arboard printing a debug-build warning directly over Ratatui's screen when a
// freshly written handle is dropped. Serializing access also avoids the
// platform contention arboard documents on Windows.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn with_clipboard<T>(operation: impl FnOnce(&mut arboard::Clipboard) -> Result<T>) -> Result<T> {
    let mut clipboard = CLIPBOARD
        .lock()
        .map_err(|_| anyhow::anyhow!("system clipboard lock was poisoned"))?;
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().context("open system clipboard")?);
    }
    operation(clipboard.as_mut().expect("clipboard was initialized"))
}

/// Read the clipboard, preferring a PNG image whenever one is present.
///
/// On WSL this invokes Windows PowerShell in a single-threaded apartment, as
/// required by `System.Windows.Forms.Clipboard`, and captures PNG bytes on
/// stdout. The command has no user-provided arguments or shell interpolation.
pub fn read() -> Result<ClipboardContent> {
    #[cfg(target_os = "linux")]
    if running_under_wsl() {
        return read_wsl_clipboard();
    }
    read_native_clipboard()
}

/// Read text for fields that must never receive image content, such as an ACP
/// elicitation answer.
pub fn read_text() -> Result<String> {
    #[cfg(target_os = "linux")]
    if running_under_wsl() {
        return match read_wsl_clipboard_with_script(WSL_TEXT_SCRIPT)? {
            ClipboardContent::Text(text) => Ok(text),
            ClipboardContent::Image(_) => bail!("clipboard contains an image, not text"),
        };
    }
    with_clipboard(|clipboard| {
        clipboard
            .get_text()
            .context("read text from system clipboard")
    })
}

/// Writes `text` to the system clipboard.
///
/// Callers must run this off the render loop: opening the platform clipboard
/// blocks.
pub fn write_text(text: &str) -> Result<()> {
    with_clipboard(|clipboard| {
        clipboard
            .set_text(text)
            .context("write text to system clipboard")
    })
}

fn read_native_clipboard() -> Result<ClipboardContent> {
    with_clipboard(|clipboard| {
        // arboard's image API returns raw RGBA pixels. Convert them to PNG so
        // the ACP payload is portable and matches the WSL path.
        if let Ok(image) = clipboard.get_image() {
            return encode_native_image(image).map(ClipboardContent::Image);
        }
        let text = clipboard
            .get_text()
            .context("read text from system clipboard")?;
        if text.is_empty() {
            bail!("clipboard contains neither an image nor text");
        }
        Ok(ClipboardContent::Text(text))
    })
}

fn encode_native_image(image: arboard::ImageData<'_>) -> Result<ClipboardImage> {
    let width = u32::try_from(image.width).context("clipboard image width is too large")?;
    let height = u32::try_from(image.height).context("clipboard image height is too large")?;
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("clipboard image dimensions overflow")?;
    if expected > MAX_DECODED_IMAGE_BYTES {
        bail!("clipboard image expands beyond the supported pixel budget");
    }
    if image.bytes.len() != expected {
        bail!("clipboard image has invalid RGBA data");
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("start PNG encoding for clipboard image")?;
        writer
            .write_image_data(image.bytes.as_ref())
            .context("encode clipboard image as PNG")?;
    }
    if png.len() > MAX_IMAGE_BYTES {
        bail!("clipboard image is too large (maximum {MAX_IMAGE_BYTES} bytes)");
    }
    Ok(ClipboardImage {
        data_base64: base64::engine::general_purpose::STANDARD.encode(png),
        mime_type: "image/png".to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn running_under_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|release| {
            let release = release.to_ascii_lowercase();
            release.contains("microsoft") || release.contains("wsl")
        })
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard() -> Result<ClipboardContent> {
    read_wsl_clipboard_with_script(WSL_CLIPBOARD_SCRIPT)
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard_with_script(script: &str) -> Result<ClipboardContent> {
    let executable = wsl_powershell_executable()
        .context("Windows PowerShell is unavailable; cannot read the WSL clipboard")?;
    let script = format!("$maxImageBytes = {MAX_IMAGE_BYTES};\n{script}");
    let command = hel::hel_targets::CommandSpec::new(
        executable.to_string_lossy(),
        [
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-Command",
            script.as_str(),
        ],
    )
    .purpose("read Windows clipboard image or text");
    let output = hel::hel_targets::CancellableProcessExecutor::with_timeout(WSL_CLIPBOARD_TIMEOUT)
        .execute(&command)
        .context("read clipboard through Windows PowerShell")?;
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if detail.is_empty() {
            bail!(
                "Windows clipboard helper exited with status {}",
                output.status
            );
        }
        if detail.contains("ExternalException") {
            bail!(
                "Windows clipboard is unavailable after retries; try again from an unlocked Windows desktop"
            );
        }
        bail!("Windows clipboard helper failed: {detail}");
    }
    parse_wsl_clipboard_output(&output.stdout)
}

#[cfg(target_os = "linux")]
fn wsl_powershell_executable() -> Option<std::path::PathBuf> {
    // Prefer the mounted Windows path. The bare name remains useful when a
    // distribution has imported the Windows PATH but mounted drives differ.
    [
        Path::new("/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"),
        Path::new("/mnt/c/Windows/SysNative/WindowsPowerShell/v1.0/powershell.exe"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
    .map(Path::to_path_buf)
    .or_else(|| Some(std::path::PathBuf::from("powershell.exe")))
}

/// Decode the line-oriented output of the WSL helper. Kept public for focused
/// behavioral tests without touching a host clipboard.
pub fn parse_wsl_clipboard_output(output: &[u8]) -> Result<ClipboardContent> {
    let output =
        String::from_utf8(output.to_vec()).context("clipboard helper output was not UTF-8")?;
    let Some((kind, payload)) = output.split_once('\n') else {
        bail!("Windows clipboard helper returned an unsupported format");
    };
    match kind.trim_end_matches('\r') {
        "IMAGE" => {
            let payload = payload.trim_end_matches(['\r', '\n']);
            ClipboardImage::from_png_base64(payload.to_owned()).map(ClipboardContent::Image)
        }
        "TEXT" => Ok(ClipboardContent::Text(payload.to_owned())),
        "EMPTY" if payload.is_empty() => {
            bail!("clipboard contains neither an image nor text")
        }
        _ => bail!("Windows clipboard helper returned an unsupported format"),
    }
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>> {
    if encoded.is_empty() {
        bail!("clipboard image payload is empty");
    }
    if encoded.len() > MAX_IMAGE_BASE64_BYTES {
        bail!("clipboard image is too large (maximum {MAX_IMAGE_BYTES} bytes)");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("decode clipboard image")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("clipboard image is too large (maximum {MAX_IMAGE_BYTES} bytes)");
    }
    Ok(bytes)
}

fn validate_png(bytes: &[u8]) -> Result<()> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("clipboard image is not a PNG");
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
    let reader = decoder.read_info().context("decode clipboard PNG header")?;
    let output_size = reader
        .output_buffer_size()
        .context("clipboard PNG has invalid dimensions")?;
    if output_size > MAX_DECODED_IMAGE_BYTES {
        bail!("clipboard image expands beyond the supported pixel budget");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn decodes_png_payload_larger_than_a_pipe_buffer() {
        let png = noisy_png();
        assert!(png.len() > 64 * 1024);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        let directory = tempfile::tempdir().expect("create fixture directory");
        let fixture = directory.path().join("output");
        let mut helper_output = Vec::with_capacity(6 + encoded.len());
        helper_output.extend_from_slice(b"IMAGE\n");
        helper_output.extend_from_slice(encoded.as_bytes());
        std::fs::write(&fixture, helper_output).expect("write clipboard helper fixture");
        let mut command =
            hel::hel_targets::CommandSpec::new("cat", [fixture.to_string_lossy().into_owned()]);
        command = command.purpose("test large clipboard image helper");
        let process =
            hel::hel_targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(3));
        let helper_output = process
            .execute(&command)
            .expect("large clipboard helper should finish");
        assert!(helper_output.status == 0);
        let content = parse_wsl_clipboard_output(&helper_output.stdout)
            .expect("large image payload should decode");
        let ClipboardContent::Image(image) = content else {
            panic!("expected image clipboard content");
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image.data_base64)
                .unwrap(),
            png,
        );
    }

    #[cfg(unix)]
    fn noisy_png() -> Vec<u8> {
        let width = 256_u32;
        let height = 256_u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        let mut state = 0x1234_5678_u32;
        for _ in 0..(width * height * 4) as usize {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.push((state >> 24) as u8);
        }
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
        }
        png
    }

    #[test]
    fn rejects_empty_and_unsupported_clipboard_output() {
        assert!(parse_wsl_clipboard_output(b"EMPTY\n").is_err());
        assert!(parse_wsl_clipboard_output(b"HTML\n<body>").is_err());
    }

    #[test]
    fn preserves_text_clipboard_output_verbatim() {
        let content = parse_wsl_clipboard_output(b"TEXT\n/plan keep this literal\n").unwrap();
        assert_eq!(
            content,
            ClipboardContent::Text("/plan keep this literal\n".into())
        );
    }
}
