// Port of mj-chat/src/clipboard.rs.

// Small cross-platform clipboard boundary shared by terminal text editors.
//
// Clipboard access is deliberately kept behind this module. Opening a native
// clipboard can block, and WSL does not expose the Windows clipboard through
// the Linux clipboard libraries the native reader normally uses.

package chat

import (
	"encoding/json"
	"fmt"
	"runtime"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PasteShortcut is the platform's primary terminal clipboard shortcut,
// shared by help and footers.
var PasteShortcut = func() string {
	if runtime.GOOS == "darwin" {
		return "Cmd-V"
	}
	return "Ctrl-V"
}()

// MaxImageBytes keeps one image comfortably below the one-megabyte durable
// relay command budget after PNG base64 encoding and JSON framing are added.
const MaxImageBytes = core.MaxImageBytes

const maxImageBase64Bytes = (MaxImageBytes + 2) / 3 * 4
const maxDecodedImageBytes = 256 * 1024 * 1024
const pendingImageMimeType = "application/x-mjolnir-pending"
const failedImageMimeType = "application/x-mjolnir-failed"

// wslClipboardTimeout bounds the Windows PowerShell clipboard helper (Linux
// only, under WSL).
const wslClipboardTimeout = 10 * time.Second

const wslImageEncoderScript = `
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

            $resizedImage = Resize-ImageHighQuality ` + "`" + `
                -Image $currentImage ` + "`" + `
                -Width $newWidth ` + "`" + `
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
`

const wslClipboardScript = `
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
for ($attempt = 0; $attempt -lt 3; $attempt++) {
    try {
        $kind = $null
        $payload = $null
        if ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
            # Copied files paste as their paths, one per line, quoted when
            # they hold whitespace, as the native reader writes them.
            $kind = 'TEXT'
            $paths = foreach ($path in [System.Windows.Forms.Clipboard]::GetFileDropList()) {
                if ($path -match '\s') { '"' + $path + '"' } else { $path }
            }
            $payload = $paths -join "` + "`" + `n"
        } elseif ([System.Windows.Forms.Clipboard]::ContainsImage()) {
            $image = [System.Windows.Forms.Clipboard]::GetImage()
            if ($null -eq $image) { throw 'clipboard image disappeared while reading' }
            try {
                $pngBytes = Convert-ImageToPngBytes ` + "`" + `
                    -Image $image ` + "`" + `
                    -MaxImageBytes $maxImageBytes ` + "`" + `
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
        [Console]::Out.Write("$kind` + "`" + `n")
        [Console]::Out.Write($payload)
        break
    } catch [System.Runtime.InteropServices.ExternalException] {
        if ($attempt -eq 2) { throw }
        Start-Sleep -Milliseconds 50
    }
}
`

const wslTextScript = `
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
for ($attempt = 0; $attempt -lt 3; $attempt++) {
    try {
        if ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
            # Copied files paste as their paths, one per line, quoted when
            # they hold whitespace, as the native reader writes them.
            $kind = 'TEXT'
            $paths = foreach ($path in [System.Windows.Forms.Clipboard]::GetFileDropList()) {
                if ($path -match '\s') { '"' + $path + '"' } else { $path }
            }
            $payload = $paths -join "` + "`" + `n"
        } elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {
            $kind = 'TEXT'
            $payload = [System.Windows.Forms.Clipboard]::GetText()
        } else {
            $kind = 'EMPTY'
            $payload = ''
        }
        [Console]::Out.Write("$kind` + "`" + `n")
        [Console]::Out.Write($payload)
        break
    } catch [System.Runtime.InteropServices.ExternalException] {
        if ($attempt -eq 2) { throw }
        Start-Sleep -Milliseconds 50
    }
}
`

const wslWriteTextScript = `
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$bytes = [Convert]::FromBase64String([Console]::In.ReadToEnd())
$text = [System.Text.Encoding]::UTF8.GetString($bytes)
for ($attempt = 0; $attempt -lt 3; $attempt++) {
    try {
        if ($text.Length -eq 0) {
            [System.Windows.Forms.Clipboard]::Clear()
        } else {
            [System.Windows.Forms.Clipboard]::SetText($text)
        }
        break
    } catch [System.Runtime.InteropServices.ExternalException] {
        if ($attempt -eq 2) { throw }
        Start-Sleep -Milliseconds 50
    }
}
`

// ClipboardImage is either a legacy inline ACP image or a native,
// session-scoped attachment reference. Inline bytes are retained while an
// image is being normalized; durable prompts and drafts use Reference.
type ClipboardImage struct {
	// DataBase64 is the base64-encoded PNG bytes.
	DataBase64 string `json:"data_base64,omitempty"`
	MimeType   string `json:"mime_type"`
	// Reference is an immutable image installed in the session attachment
	// store.
	Reference *core.AttachmentRef `json:"reference,omitempty"`
}

// UnmarshalJSON requires mime_type, as serde does.
func (i *ClipboardImage) UnmarshalJSON(data []byte) error {
	if err := clipboardRequireFields(data, "mime_type"); err != nil {
		return err
	}
	type plain ClipboardImage
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*i = ClipboardImage(decoded)
	return nil
}

// Equal is Rust's derived PartialEq: references compare by value.
func (i *ClipboardImage) Equal(other *ClipboardImage) bool {
	if i.DataBase64 != other.DataBase64 || i.MimeType != other.MimeType {
		return false
	}
	if i.Reference == nil || other.Reference == nil {
		return i.Reference == nil && other.Reference == nil
	}
	return *i.Reference == *other.Reference
}

// clipboardRequireFields reports serde's error for the first of names that
// the JSON object lacks.
func clipboardRequireFields(data []byte, names ...string) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// clipboardImagePending is a marker used while a background attachment task
// is still running.
func clipboardImagePending() ClipboardImage {
	return ClipboardImage{MimeType: pendingImageMimeType}
}

// clipboardImageFailed is a removable marker left when an attachment task
// failed.
func clipboardImageFailed() ClipboardImage {
	return ClipboardImage{MimeType: failedImageMimeType}
}

func (i *ClipboardImage) isPlaceholder() bool {
	return i.Reference == nil &&
		i.DataBase64 == "" &&
		(i.MimeType == pendingImageMimeType || i.MimeType == failedImageMimeType)
}

func (i *ClipboardImage) isPending() bool {
	return i.MimeType == pendingImageMimeType
}

// ClipboardImageFromPNGBase64 constructs and validates an image from encoded
// PNG bytes.
func ClipboardImageFromPNGBase64(dataBase64 string) (ClipboardImage, error) {
	return ClipboardImageFromBase64(dataBase64, "image/png")
}

// ClipboardImageFromBase64 constructs an embedded image from a serialized
// ACP image block. PNG headers are bounded; other image media types are
// retained when their base64 bytes are valid so queued prompts from the web
// surface are not silently downgraded to text during terminal editing.
func ClipboardImageFromBase64(dataBase64 string, mimeType string) (ClipboardImage, error) {
	panic("unported: mj-chat/src/clipboard.rs ClipboardImageFromBase64")
}

// ClipboardImageFromReference constructs an image that carries only its
// session-scoped reference.
func ClipboardImageFromReference(reference core.AttachmentRef) (ClipboardImage, error) {
	if err := reference.Validate(); err != nil {
		return ClipboardImage{}, err
	}
	return ClipboardImage{MimeType: reference.MimeType, Reference: &reference}, nil
}

func (i *ClipboardImage) IsReference() bool {
	return i.Reference != nil
}

// ClipboardContent is the clipboard value selected for the caller. An image
// wins when both representations are available; callers that edit ordinary
// text fields can explicitly accept only ClipboardContentText.
type ClipboardContent interface {
	isClipboardContent()
}

type ClipboardContentText struct {
	Text string
}

type ClipboardContentImage struct {
	Image ClipboardImage
}

func (ClipboardContentText) isClipboardContent()  {}
func (ClipboardContentImage) isClipboardContent() {}

// clipboardImageData is the native clipboard's raw RGBA image (Rust's
// arboard::ImageData).
type clipboardImageData struct {
	width  int
	height int
	bytes  []byte
}

// nativeClipboard keeps the platform access boundary small so format
// precedence can be tested without replacing the user's actual desktop
// clipboard. Each method reports errClipboardContentNotAvailable when the
// clipboard holds no content of that kind.
type nativeClipboard interface {
	getImage() (clipboardImageData, error)
	getText() (string, error)
	getFileList() ([]string, error)
}

// errClipboardContentNotAvailable is arboard's ContentNotAvailable.
var errClipboardContentNotAvailable = fmt.Errorf("the clipboard contents were not available in the requested format or the clipboard is empty")

// systemClipboard is the desktop clipboard handle.
type systemClipboard struct{}

func openSystemClipboard() (*systemClipboard, error) {
	panic("unported: mj-chat/src/clipboard.rs openSystemClipboard")
}

func (c *systemClipboard) getImage() (clipboardImageData, error) {
	panic("unported: mj-chat/src/clipboard.rs getImage")
}

func (c *systemClipboard) getText() (string, error) {
	panic("unported: mj-chat/src/clipboard.rs getText")
}

func (c *systemClipboard) getFileList() ([]string, error) {
	panic("unported: mj-chat/src/clipboard.rs getFileList")
}

func (c *systemClipboard) setText(text string) error {
	panic("unported: mj-chat/src/clipboard.rs setText")
}

// Linux clipboards are owned by the process that last supplied their
// contents. Keeping one handle alive prevents copied text from disappearing.
// Serializing access also avoids the platform contention documented on
// Windows.
var (
	clipboardMu     sync.Mutex
	clipboardHandle *systemClipboard
)

func withClipboard[T any](operation func(*systemClipboard) (T, error)) (T, error) {
	clipboardMu.Lock()
	defer clipboardMu.Unlock()
	if clipboardHandle == nil {
		handle, err := openSystemClipboard()
		if err != nil {
			var zero T
			return zero, fmt.Errorf("open system clipboard: %w", err)
		}
		clipboardHandle = handle
	}
	return operation(clipboardHandle)
}

// Read reads the clipboard, preferring an image whenever one is present.
//
// On WSL this invokes Windows PowerShell in a single-threaded apartment, as
// required by System.Windows.Forms.Clipboard, and captures PNG bytes on
// stdout. The command has no user-provided arguments or shell interpolation.
func Read() (ClipboardContent, error) {
	if runningUnderWSL() {
		return readWSLClipboard()
	}
	return readNativeClipboard()
}

// ReadText reads text for fields that must never receive image content,
// such as an ACP elicitation answer.
func ReadText() (string, error) {
	panic("unported: mj-chat/src/clipboard.rs ReadText")
}

// WriteText writes text to the system clipboard.
//
// On WSL, PowerShell writes directly to Windows without a WSLg connection.
// Callers must run this off the render loop: opening the platform clipboard
// blocks.
func WriteText(text string) error {
	panic("unported: mj-chat/src/clipboard.rs WriteText")
}

func writeWSLClipboard(text string) error {
	panic("unported: mj-chat/src/clipboard.rs writeWSLClipboard")
}

func writeWSLClipboardWithCommand(text string, command core.CommandSpec) error {
	panic("unported: mj-chat/src/clipboard.rs writeWSLClipboardWithCommand")
}

func readNativeClipboard() (ClipboardContent, error) {
	return withClipboard(func(clipboard *systemClipboard) (ClipboardContent, error) {
		return readNativeContent(clipboard, false)
	})
}

func readNativeContent(clipboard nativeClipboard, textOnly bool) (ClipboardContent, error) {
	panic("unported: mj-chat/src/clipboard.rs readNativeContent")
}

// fileListText is copied files as pasted text: one path per line, quoted
// when it holds whitespace, which the composer reads back as attachable
// paths.
func fileListText(paths []string) string {
	panic("unported: mj-chat/src/clipboard.rs fileListText")
}

func encodeNativeImage(image clipboardImageData) (ClipboardImage, error) {
	panic("unported: mj-chat/src/clipboard.rs encodeNativeImage")
}

// runningUnderWSL reports a Linux process running under WSL; false on every
// other platform.
func runningUnderWSL() bool {
	panic("unported: mj-chat/src/clipboard.rs runningUnderWSL")
}

func readWSLClipboard() (ClipboardContent, error) {
	return readWSLClipboardWithScript(wslClipboardScript)
}

func readWSLClipboardWithScript(script string) (ClipboardContent, error) {
	panic("unported: mj-chat/src/clipboard.rs readWSLClipboardWithScript")
}

// NormalizeImage normalizes an encoded clipboard image through the
// controller's shared image optimizer. The PowerShell WSL helper already
// applies a conservative resize, but running the common optimizer here keeps
// its MIME type, dimensions, and byte bound identical to native clipboard
// images.
func NormalizeImage(image ClipboardImage) (ClipboardImage, error) {
	panic("unported: mj-chat/src/clipboard.rs NormalizeImage")
}

func composeWSLScript(script string) string {
	return fmt.Sprintf("$maxImageBytes = %d;\n$maxDecodedImageBytes = %d;\n%s\n%s",
		MaxImageBytes, maxDecodedImageBytes, wslImageEncoderScript, script)
}

func wslPowershellExecutable() (string, bool) {
	panic("unported: mj-chat/src/clipboard.rs wslPowershellExecutable")
}

// ParseWSLClipboardOutput decodes the line-oriented output of the WSL
// helper. Kept public for focused behavioral tests without touching a host
// clipboard.
func ParseWSLClipboardOutput(output []byte) (ClipboardContent, error) {
	panic("unported: mj-chat/src/clipboard.rs ParseWSLClipboardOutput")
}

func decodeBase64(encoded string) ([]byte, error) {
	panic("unported: mj-chat/src/clipboard.rs decodeBase64")
}

func validatePNG(bytes []byte) error {
	panic("unported: mj-chat/src/clipboard.rs validatePNG")
}
