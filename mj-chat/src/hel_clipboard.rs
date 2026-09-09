//! Small cross-platform clipboard boundary shared by terminal text editors.
//!
//! Clipboard access is deliberately kept behind this module. Opening a native
//! clipboard can block, and WSL does not expose the Windows clipboard through
//! the Linux clipboard libraries that [`arboard`] normally uses.

#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::{Arc, Mutex};
#[cfg(any(target_os = "linux", all(test, unix)))]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
#[cfg(any(target_os = "linux", all(test, unix)))]
use hel::hel_targets::CommandExecutor;
use serde::{Deserialize, Serialize};

use hel::hel_attachment::AttachmentRef;

/// Keep one image comfortably below the one-megabyte durable relay command
/// budget after PNG base64 encoding and JSON framing are added.
pub const MAX_IMAGE_BYTES: usize = hel::hel_attachment::MAX_IMAGE_BYTES;
const MAX_IMAGE_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_DECODED_IMAGE_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const PENDING_IMAGE_MIME_TYPE: &str = "application/x-mjolnir-pending";
pub(crate) const FAILED_IMAGE_MIME_TYPE: &str = "application/x-mjolnir-failed";
#[cfg(target_os = "linux")]
const WSL_CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const WSL_IMAGE_ENCODER_SCRIPT: &str = r#"
function Resize-ImageHighQuality {
    param(
        [Parameter(Mandatory = $true)]
        [System.Drawing.Image] $Image,
        [Parameter(Mandatory = $true)]
        [int] $Width,
        [Parameter(Mandatory = $true)]
        [int] $Height
    )

    $resizedImage = [System.Drawing.Bitmap]::new(
        $Width,
        $Height,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = $null
    try {
        $graphics = [System.Drawing.Graphics]::FromImage($resizedImage)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.DrawImage(
            $Image,
            [System.Drawing.Rectangle]::new(0, 0, $Width, $Height),
            0,
            0,
            $Image.Width,
            $Image.Height,
            [System.Drawing.GraphicsUnit]::Pixel
        )
    } catch {
        $resizedImage.Dispose()
        throw
    } finally {
        if ($null -ne $graphics) {
            $graphics.Dispose()
        }
    }
    return ,$resizedImage
}

function Convert-ImageToPngBytes {
    param(
        [Parameter(Mandatory = $true)]
        [System.Drawing.Image] $Image,
        [Parameter(Mandatory = $true)]
        [long] $MaxImageBytes,
        [Parameter(Mandatory = $true)]
        [long] $MaxDecodedImageBytes
    )

    $currentImage = $Image
    $ownsCurrentImage = $false
    try {
        while ($true) {
            $decodedBytes = [double]$currentImage.Width * [double]$currentImage.Height * 4
            if ($decodedBytes -gt $MaxDecodedImageBytes) {
                $scale = [Math]::Min(0.9, [Math]::Sqrt($MaxDecodedImageBytes / $decodedBytes))
            } else {
                $stream = [System.IO.MemoryStream]::new()
                try {
                    $currentImage.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
                    if ($stream.Length -le $MaxImageBytes) {
                        return ,$stream.ToArray()
                    }
                    # Leave headroom so nearly fitting PNGs do not shrink one pixel at a time.
                    $scale = [Math]::Min(0.9, [Math]::Sqrt($MaxImageBytes / [double]$stream.Length))
                } finally {
                    $stream.Dispose()
                }
            }

            $newWidth = [Math]::Max(1, [int][Math]::Floor([double]$currentImage.Width * $scale))
            $newHeight = [Math]::Max(1, [int][Math]::Floor([double]$currentImage.Height * $scale))
            if ($newWidth -eq $currentImage.Width -and $newHeight -eq $currentImage.Height) {
                if ($currentImage.Width -gt 1) {
                    $newWidth = $currentImage.Width - 1
                } elseif ($currentImage.Height -gt 1) {
                    $newHeight = $currentImage.Height - 1
                } else {
                    throw 'clipboard image cannot be reduced below one pixel'
                }
            }

            $resizedImage = Resize-ImageHighQuality `
                -Image $currentImage `
                -Width $newWidth `
                -Height $newHeight
            if ($ownsCurrentImage) {
                $currentImage.Dispose()
            }
            $currentImage = $resizedImage
            $ownsCurrentImage = $true
        }
    } finally {
        if ($ownsCurrentImage) {
            $currentImage.Dispose()
        }
    }
}
"#;
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
            try {
                $pngBytes = Convert-ImageToPngBytes `
                    -Image $image `
                    -MaxImageBytes $maxImageBytes `
                    -MaxDecodedImageBytes $maxDecodedImageBytes
                $kind = 'IMAGE'
                $payload = [Convert]::ToBase64String([byte[]]$pngBytes)
            } finally {
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

fn arc_str_is_empty(value: &Arc<str>) -> bool {
    value.is_empty()
}

/// A clipboard image is either a legacy inline ACP image or a native,
/// session-scoped attachment reference. Inline bytes are retained while an
/// image is being normalized; durable prompts and drafts use `reference`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardImage {
    /// Base64-encoded PNG bytes.
    #[serde(default, skip_serializing_if = "arc_str_is_empty")]
    pub data_base64: Arc<str>,
    pub mime_type: String,
    /// An immutable image installed in the session attachment store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<AttachmentRef>,
}

impl ClipboardImage {
    /// A marker used while a background attachment task is still running.
    pub(crate) fn pending() -> Self {
        Self {
            data_base64: Arc::from(""),
            mime_type: PENDING_IMAGE_MIME_TYPE.to_owned(),
            reference: None,
        }
    }

    /// A removable marker left when an attachment task failed.
    pub(crate) fn failed() -> Self {
        Self {
            data_base64: Arc::from(""),
            mime_type: FAILED_IMAGE_MIME_TYPE.to_owned(),
            reference: None,
        }
    }

    #[must_use]
    pub(crate) fn is_placeholder(&self) -> bool {
        self.reference.is_none()
            && self.data_base64.is_empty()
            && (self.mime_type == PENDING_IMAGE_MIME_TYPE
                || self.mime_type == FAILED_IMAGE_MIME_TYPE)
    }

    #[must_use]
    pub(crate) fn is_pending(&self) -> bool {
        self.mime_type == PENDING_IMAGE_MIME_TYPE
    }

    /// Construct and validate an image from encoded PNG bytes.
    pub fn from_png_base64(data_base64: impl Into<Arc<str>>) -> Result<Self> {
        Self::from_base64(data_base64, "image/png".to_owned())
    }

    /// Construct an embedded image from a serialized ACP image block. PNG
    /// headers are bounded; other image media types are retained when their base64
    /// bytes are valid so queued prompts from the web surface are not silently
    /// downgraded to text during terminal editing.
    pub fn from_base64(data_base64: impl Into<Arc<str>>, mime_type: String) -> Result<Self> {
        let data_base64 = data_base64.into();
        let bytes = decode_base64(&data_base64)?;
        if mime_type.eq_ignore_ascii_case("image/png") {
            validate_png(&bytes)?;
        } else if !mime_type.to_ascii_lowercase().starts_with("image/") {
            bail!("clipboard image has unsupported media type {mime_type}");
        }
        Ok(Self {
            data_base64,
            mime_type,
            reference: None,
        })
    }

    /// Construct an image that carries only its session-scoped reference.
    pub fn from_reference(reference: AttachmentRef) -> Result<Self> {
        reference.validate()?;
        Ok(Self {
            data_base64: Arc::from(""),
            mime_type: reference.mime_type.clone(),
            reference: Some(reference),
        })
    }

    #[must_use]
    pub fn is_reference(&self) -> bool {
        self.reference.is_some()
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
    let optimized = mj_client::image::optimize_rgba(width, height, image.bytes.as_ref())
        .context("optimize clipboard image")?;
    Ok(ClipboardImage {
        data_base64: base64::engine::general_purpose::STANDARD
            .encode(optimized.bytes)
            .into(),
        mime_type: optimized.mime_type,
        reference: None,
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
    let script = compose_wsl_script(script);
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
    let content = parse_wsl_clipboard_output(&output.stdout)?;
    match content {
        ClipboardContent::Image(image) => Ok(ClipboardContent::Image(normalize_image(image)?)),
        content => Ok(content),
    }
}

/// Normalize an encoded clipboard image through the controller's shared image
/// optimizer. The PowerShell WSL helper already applies a conservative resize,
/// but running the common optimizer here keeps its MIME type, dimensions, and
/// byte bound identical to native clipboard images.
pub fn normalize_image(image: ClipboardImage) -> Result<ClipboardImage> {
    if image.reference.is_some() {
        return Ok(image);
    }
    let bytes = decode_base64(&image.data_base64)?;
    let optimized = mj_client::image::optimize_image(&bytes).context("optimize clipboard image")?;
    Ok(ClipboardImage {
        data_base64: base64::engine::general_purpose::STANDARD
            .encode(optimized.bytes)
            .into(),
        mime_type: optimized.mime_type,
        reference: None,
    })
}

#[cfg(target_os = "linux")]
fn compose_wsl_script(script: &str) -> String {
    format!(
        "$maxImageBytes = {MAX_IMAGE_BYTES};\n$maxDecodedImageBytes = {MAX_DECODED_IMAGE_BYTES};\n{WSL_IMAGE_ENCODER_SCRIPT}\n{script}"
    )
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
                .decode(image.data_base64.as_bytes())
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

    #[cfg(target_os = "linux")]
    #[test]
    fn downsizes_oversized_synthetic_image_in_powershell() {
        let Some(executable) = wsl_powershell_executable() else {
            return;
        };
        if !executable.is_file() {
            // The fallback executable name can only be checked from a WSL
            // environment, where the Windows PATH is available to tests.
            return;
        }
        let script = compose_wsl_script(
            r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$width = 768
$height = 768
$bitmap = [System.Drawing.Bitmap]::new(
    $width,
    $height,
    [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
)
$rectangle = [System.Drawing.Rectangle]::new(0, 0, $width, $height)
$bitmapData = $null
try {
    $bitmapData = $bitmap.LockBits(
        $rectangle,
        [System.Drawing.Imaging.ImageLockMode]::WriteOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $pixels = [byte[]]::new($width * $height * 4)
    [Random]::new(20260906).NextBytes($pixels)
    [System.Runtime.InteropServices.Marshal]::Copy(
        $pixels,
        0,
        $bitmapData.Scan0,
        $pixels.Length
    )
} finally {
    if ($null -ne $bitmapData) {
        $bitmap.UnlockBits($bitmapData)
    }
}
try {
    [byte[]]$pngBytes = Convert-ImageToPngBytes `
        -Image $bitmap `
        -MaxImageBytes $maxImageBytes `
        -MaxDecodedImageBytes $maxDecodedImageBytes
    $resultStream = [System.IO.MemoryStream]::new($pngBytes)
    try {
        $resultImage = [System.Drawing.Image]::FromStream($resultStream)
        try {
            [Console]::Out.Write("$($pngBytes.Length)|$($bitmap.Width)|$($bitmap.Height)|$($resultImage.Width)|$($resultImage.Height)")
        } finally {
            $resultImage.Dispose()
        }
    } finally {
        $resultStream.Dispose()
    }
} finally {
    $bitmap.Dispose()
}
"#,
        );
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
        .purpose("test Windows clipboard image resizing");
        let output =
            hel::hel_targets::CancellableProcessExecutor::with_timeout(WSL_CLIPBOARD_TIMEOUT)
                .execute(&command)
                .expect("PowerShell image resize fixture should finish");
        assert_eq!(
            output.status,
            0,
            "PowerShell image resize fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fields: Vec<usize> = String::from_utf8(output.stdout)
            .expect("PowerShell fixture output should be UTF-8")
            .split('|')
            .map(|field| {
                field
                    .parse()
                    .expect("PowerShell fixture field should be numeric")
            })
            .collect();
        assert_eq!(fields.len(), 5);
        assert!(fields[0] <= MAX_IMAGE_BYTES);
        assert_eq!((fields[1], fields[2]), (768, 768));
        assert!(fields[3] < fields[1] || fields[4] < fields[2]);
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
