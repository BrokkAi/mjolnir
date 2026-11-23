// Port of mj-chat/src/clipboard.rs.

// Small cross-platform clipboard boundary shared by terminal text editors.
//
// Clipboard access is deliberately kept behind this module. Opening a native
// clipboard can block, and WSL does not expose the Windows clipboard through
// the Linux clipboard libraries the native reader normally uses.

package chat

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	"image/draw"
	"image/png"
	"log/slog"
	"math"
	"math/bits"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	bytes, err := decodeBase64(dataBase64)
	if err != nil {
		return ClipboardImage{}, err
	}
	if textASCIIEqualFold(mimeType, "image/png") {
		if err := validatePNG(bytes); err != nil {
			return ClipboardImage{}, err
		}
	} else if !strings.HasPrefix(textASCIILower(mimeType), "image/") {
		return ClipboardImage{}, fmt.Errorf("clipboard image has unsupported media type %s", mimeType)
	}
	return ClipboardImage{DataBase64: dataBase64, MimeType: mimeType}, nil
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

// clipboardTool is the family of desktop clipboard programs the system
// clipboard drives. Rust's arboard talks to the platform clipboard in
// process; Go has no such library without cgo, so the port drives each
// platform's own clipboard tools. Those also keep copied text alive after
// Hel exits, which an in-process X11 or Wayland owner cannot.
type clipboardTool int

const (
	// clipboardToolWayland is wl-clipboard's wl-paste and wl-copy.
	clipboardToolWayland clipboardTool = iota
	// clipboardToolX11 is xclip.
	clipboardToolX11
	// clipboardToolMacOS is pbpaste, pbcopy and osascript.
	clipboardToolMacOS
	// clipboardToolWindows is Windows PowerShell.
	clipboardToolWindows
)

// systemClipboard is the desktop clipboard handle.
type systemClipboard struct {
	tool clipboardTool
}

// clipboardNotAvailablePatterns are the messages the Linux tools print when
// the clipboard is empty or lacks the requested type, as opposed to failing.
var clipboardNotAvailablePatterns = []string{
	"Nothing is copied", "No selection", "No suitable type", "not available",
}

// clipboardTextTypes are the MIME types and X11 targets that carry text.
var clipboardTextTypes = []string{
	"text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT",
}

const macOSFileListScript = `ObjC.import("AppKit");
var urls = $.NSPasteboard.generalPasteboard.readObjectsForClassesOptions(
    $([$.NSURL]), $({"NSPasteboardURLReadingFileURLsOnlyKey": true}));
var paths = [];
if (urls) {
    for (var i = 0; i < urls.count; i++) {
        paths.push(ObjC.unwrap(urls.objectAtIndex(i).path));
    }
}
paths.join("\n");`

// windowsClipboardPrelude starts every native Windows clipboard read. Exit
// status 3 means the clipboard holds no content of the requested kind.
const windowsClipboardPrelude = `$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
`

const windowsTextScript = windowsClipboardPrelude + `if (-not [System.Windows.Forms.Clipboard]::ContainsText()) { exit 3 }
[Console]::Out.Write([System.Windows.Forms.Clipboard]::GetText())
`

const windowsImageScript = windowsClipboardPrelude + `if (-not [System.Windows.Forms.Clipboard]::ContainsImage()) { exit 3 }
$image = [System.Windows.Forms.Clipboard]::GetImage()
if ($null -eq $image) { exit 3 }
$stream = [System.IO.MemoryStream]::new()
try {
    $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))
} finally {
    $stream.Dispose()
    $image.Dispose()
}
`

const windowsFileListScript = windowsClipboardPrelude + `if (-not [System.Windows.Forms.Clipboard]::ContainsFileDropList()) { exit 3 }
$paths = @([System.Windows.Forms.Clipboard]::GetFileDropList())
[Console]::Out.Write($paths -join "` + "`" + `n")
`

const windowsClipboardNotAvailableStatus = 3

func openSystemClipboard() (*systemClipboard, error) {
	switch runtime.GOOS {
	case "darwin":
		return &systemClipboard{tool: clipboardToolMacOS}, nil
	case "windows":
		return &systemClipboard{tool: clipboardToolWindows}, nil
	}
	if os.Getenv("WAYLAND_DISPLAY") != "" && clipboardProgramsFound("wl-paste", "wl-copy") {
		return &systemClipboard{tool: clipboardToolWayland}, nil
	}
	if os.Getenv("DISPLAY") != "" && clipboardProgramsFound("xclip") {
		return &systemClipboard{tool: clipboardToolX11}, nil
	}
	return nil, errors.New("no desktop clipboard is reachable: run Hel in a Wayland session with wl-clipboard installed, or an X11 session with xclip installed")
}

func clipboardProgramsFound(programs ...string) bool {
	for _, program := range programs {
		if _, err := exec.LookPath(program); err != nil {
			return false
		}
	}
	return true
}

// clipboardToolOutput runs one clipboard reader to completion. An exit the
// tool uses for an empty clipboard or a missing type is
// errClipboardContentNotAvailable; any other failure carries the tool's
// message.
func clipboardToolOutput(program string, args []string, environment []string) ([]byte, error) {
	command := exec.Command(program, args...)
	if environment != nil {
		command.Env = append(os.Environ(), environment...)
	}
	output, err := core.Capture(context.Background(), command)
	if err != nil {
		return nil, fmt.Errorf("run %s: %w", program, err)
	}
	if output.Status != nil && output.Status.Success() {
		return output.Stdout, nil
	}
	detail := strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�"))
	if output.Status != nil && output.Status.ExitCode() == windowsClipboardNotAvailableStatus &&
		program == windowsPowershell {
		return nil, errClipboardContentNotAvailable
	}
	for _, pattern := range clipboardNotAvailablePatterns {
		if strings.Contains(detail, pattern) {
			return nil, errClipboardContentNotAvailable
		}
	}
	status := -1
	if output.Status != nil {
		status = output.Status.ExitCode()
	}
	if detail == "" {
		return nil, fmt.Errorf("%s exited with status %d", program, status)
	}
	return nil, fmt.Errorf("%s exited with status %d: %s", program, status, detail)
}

const windowsPowershell = "powershell.exe"

func windowsClipboardRead(script string) ([]byte, error) {
	return clipboardToolOutput(windowsPowershell,
		[]string{"-NoProfile", "-NonInteractive", "-Sta", "-Command", script}, nil)
}

// macOSUTF8Environment makes pbcopy and pbpaste exchange UTF-8 whatever the
// caller's locale; under the C locale they mangle non-ASCII text.
var macOSUTF8Environment = []string{"LC_ALL=en_US.UTF-8", "LANG=en_US.UTF-8"}

// clipboardTypes lists the MIME types or X11 targets the clipboard offers.
func (c *systemClipboard) clipboardTypes() ([]string, error) {
	var output []byte
	var err error
	switch c.tool {
	case clipboardToolWayland:
		output, err = clipboardToolOutput("wl-paste", []string{"--list-types"}, nil)
	case clipboardToolX11:
		output, err = clipboardToolOutput("xclip", []string{"-selection", "clipboard", "-t", "TARGETS", "-o"}, nil)
	default:
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var types []string
	for _, line := range strings.Split(string(output), "\n") {
		if line = strings.TrimSpace(line); line != "" {
			types = append(types, line)
		}
	}
	return types, nil
}

// readType reads one MIME type or X11 target, which must be offered.
func (c *systemClipboard) readType(offered []string, wanted ...string) ([]byte, error) {
	for _, candidate := range wanted {
		if !slices.Contains(offered, candidate) {
			continue
		}
		if c.tool == clipboardToolWayland {
			return clipboardToolOutput("wl-paste", []string{"--no-newline", "--type", candidate}, nil)
		}
		return clipboardToolOutput("xclip", []string{"-selection", "clipboard", "-t", candidate, "-o"}, nil)
	}
	return nil, errClipboardContentNotAvailable
}

func (c *systemClipboard) getImage() (clipboardImageData, error) {
	var encoded []byte
	switch c.tool {
	case clipboardToolMacOS:
		output, err := clipboardToolOutput("osascript", []string{"-e", "the clipboard as «class PNGf»"}, nil)
		if err != nil {
			return clipboardImageData{}, errClipboardContentNotAvailable
		}
		text := strings.TrimSpace(string(output))
		hexDigits, ok := strings.CutPrefix(text, "«data PNGf")
		if !ok {
			return clipboardImageData{}, errClipboardContentNotAvailable
		}
		decoded, err := hex.DecodeString(strings.TrimSuffix(hexDigits, "»"))
		if err != nil {
			return clipboardImageData{}, fmt.Errorf("decode clipboard PNG from osascript: %w", err)
		}
		encoded = decoded
	case clipboardToolWindows:
		output, err := windowsClipboardRead(windowsImageScript)
		if err != nil {
			return clipboardImageData{}, err
		}
		decoded, err := base64.StdEncoding.DecodeString(strings.TrimSpace(string(output)))
		if err != nil {
			return clipboardImageData{}, fmt.Errorf("decode clipboard PNG from PowerShell: %w", err)
		}
		encoded = decoded
	default:
		types, err := c.clipboardTypes()
		if err != nil {
			return clipboardImageData{}, err
		}
		if encoded, err = c.readType(types, "image/png"); err != nil {
			return clipboardImageData{}, err
		}
	}
	return decodeClipboardPNG(encoded)
}

// decodeClipboardPNG turns the PNG a clipboard tool returned into the raw
// RGBA rows arboard hands out. An image beyond the decoded pixel budget is
// returned without pixels, so encodeNativeImage reports the budget instead
// of this reader decoding it.
func decodeClipboardPNG(encoded []byte) (clipboardImageData, error) {
	config, err := png.DecodeConfig(bytes.NewReader(encoded))
	if err != nil {
		return clipboardImageData{}, fmt.Errorf("decode clipboard PNG: %w", err)
	}
	if hi, pixels := bits.Mul64(uint64(config.Width), uint64(config.Height)); hi != 0 ||
		pixels > maxDecodedImageBytes/4 {
		return clipboardImageData{width: config.Width, height: config.Height}, nil
	}
	decoded, err := png.Decode(bytes.NewReader(encoded))
	if err != nil {
		return clipboardImageData{}, fmt.Errorf("decode clipboard PNG: %w", err)
	}
	bounds := decoded.Bounds()
	rgba := image.NewNRGBA(image.Rect(0, 0, bounds.Dx(), bounds.Dy()))
	draw.Draw(rgba, rgba.Bounds(), decoded, bounds.Min, draw.Src)
	return clipboardImageData{width: bounds.Dx(), height: bounds.Dy(), bytes: rgba.Pix}, nil
}

func (c *systemClipboard) getText() (string, error) {
	var output []byte
	var err error
	switch c.tool {
	case clipboardToolMacOS:
		output, err = clipboardToolOutput("pbpaste", nil, macOSUTF8Environment)
		if err == nil && len(output) == 0 {
			return "", errClipboardContentNotAvailable
		}
	case clipboardToolWindows:
		output, err = windowsClipboardRead(windowsTextScript)
	default:
		var types []string
		if types, err = c.clipboardTypes(); err == nil {
			output, err = c.readType(types, clipboardTextTypes...)
		}
	}
	if err != nil {
		return "", err
	}
	return strings.ToValidUTF8(string(output), "�"), nil
}

func (c *systemClipboard) getFileList() ([]string, error) {
	switch c.tool {
	case clipboardToolMacOS:
		output, err := clipboardToolOutput("osascript", []string{"-l", "JavaScript", "-e", macOSFileListScript}, nil)
		if err != nil {
			return nil, err
		}
		return nonEmptyLines(string(output)), nil
	case clipboardToolWindows:
		output, err := windowsClipboardRead(windowsFileListScript)
		if err != nil {
			return nil, err
		}
		return nonEmptyLines(string(output)), nil
	}
	types, err := c.clipboardTypes()
	if err != nil {
		return nil, err
	}
	output, err := c.readType(types, "text/uri-list")
	if err != nil {
		return nil, err
	}
	var paths []string
	for _, line := range strings.Split(string(output), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		parsed, err := url.Parse(line)
		if err != nil || parsed.Scheme != "file" {
			continue
		}
		paths = append(paths, filepath.FromSlash(parsed.Path))
	}
	return paths, nil
}

func nonEmptyLines(text string) []string {
	var lines []string
	for _, line := range strings.Split(text, "\n") {
		if line = strings.TrimRight(line, "\r"); line != "" {
			lines = append(lines, line)
		}
	}
	return lines
}

func (c *systemClipboard) setText(text string) error {
	switch c.tool {
	case clipboardToolMacOS:
		command := exec.Command("pbcopy")
		command.Env = append(os.Environ(), macOSUTF8Environment...)
		output, err := core.RunWithInput(command, []byte(text))
		if err != nil {
			return fmt.Errorf("run pbcopy: %w", err)
		}
		if output.Status != nil && !output.Status.Success() {
			return fmt.Errorf("pbcopy exited with status %d: %s", output.Status.ExitCode(),
				strings.TrimSpace(string(output.Stderr)))
		}
		return nil
	case clipboardToolWindows:
		return writeWSLClipboardWithCommand(text, core.NewCommandSpec(windowsPowershell,
			[]string{"-NoProfile", "-NonInteractive", "-Sta", "-Command", wslWriteTextScript}).
			WithPurpose("write Windows clipboard text"))
	case clipboardToolWayland:
		return runClipboardOwner("wl-copy", nil, text)
	default:
		return runClipboardOwner("xclip", []string{"-selection", "clipboard", "-in"}, text)
	}
}

// runClipboardOwner hands text to wl-copy or xclip. Both read their input,
// then fork a background owner that serves the clipboard until something
// else is copied, so this waits only for the foreground process.
//
// Raw exec on purpose: the shared subprocess helpers own the child's process
// group and kill it when the call returns, and capture output through pipes
// that the forked owner would hold open. Either would lose the copied text.
// Output goes to a file, never a pipe, so nothing here can block on the
// owner.
func runClipboardOwner(program string, args []string, text string) error {
	stderr, err := os.CreateTemp("", "mj-clipboard-*.log")
	if err != nil {
		return fmt.Errorf("create %s error log: %w", program, err)
	}
	defer func() {
		_ = stderr.Close()
		if err := os.Remove(stderr.Name()); err != nil {
			slog.Debug("clipboard error log was not removed", "path", stderr.Name(), "error", err)
		}
	}()
	command := exec.Command(program, args...)
	command.Stdin = strings.NewReader(text)
	command.Stderr = stderr
	if err := command.Run(); err != nil {
		detail, readErr := os.ReadFile(stderr.Name())
		if readErr != nil {
			return fmt.Errorf("run %s: %w (its error output could not be read: %v)", program, err, readErr)
		}
		if message := strings.TrimSpace(string(detail)); message != "" {
			return fmt.Errorf("run %s: %w: %s", program, err, message)
		}
		return fmt.Errorf("run %s: %w", program, err)
	}
	return nil
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
	if runningUnderWSL() {
		content, err := readWSLClipboardWithScript(wslTextScript)
		if err != nil {
			return "", err
		}
		text, ok := content.(ClipboardContentText)
		if !ok {
			return "", errors.New("clipboard contains an image, not text")
		}
		return text.Text, nil
	}
	return withClipboard(func(clipboard *systemClipboard) (string, error) {
		content, err := readNativeContent(clipboard, true)
		if err != nil {
			return "", err
		}
		text, ok := content.(ClipboardContentText)
		if !ok {
			panic("text-only clipboard reader returned an image")
		}
		return text.Text, nil
	})
}

// WriteText writes text to the system clipboard.
//
// On WSL, PowerShell writes directly to Windows without a WSLg connection.
// Callers must run this off the render loop: opening the platform clipboard
// blocks.
func WriteText(text string) error {
	if runningUnderWSL() {
		return writeWSLClipboard(text)
	}
	_, err := withClipboard(func(clipboard *systemClipboard) (struct{}, error) {
		if err := clipboard.setText(text); err != nil {
			return struct{}{}, fmt.Errorf("write text to system clipboard: %w", err)
		}
		return struct{}{}, nil
	})
	return err
}

func writeWSLClipboard(text string) error {
	executable, ok := wslPowershellExecutable()
	if !ok {
		return errors.New("Windows PowerShell is unavailable; cannot write the WSL clipboard")
	}
	command := core.NewCommandSpec(executable,
		[]string{"-NoProfile", "-NonInteractive", "-Sta", "-Command", wslWriteTextScript}).
		WithPurpose("write Windows clipboard text")
	return writeWSLClipboardWithCommand(text, command)
}

func writeWSLClipboardWithCommand(text string, command core.CommandSpec) error {
	// Base64 preserves Unicode, leading BOM characters, and line endings
	// independently of Windows console encoding. Keep the payload out of
	// logs.
	input := []byte(base64.StdEncoding.EncodeToString([]byte(text)))
	sensitive := command.WithSensitiveStdin(input)
	output, err := core.CancellableProcessExecutorWithTimeout(wslClipboardTimeout).Execute(&sensitive)
	if err != nil {
		return fmt.Errorf("write clipboard through Windows PowerShell: %w", err)
	}
	if output.Status != 0 {
		return fmt.Errorf("Windows clipboard writer exited with status %d: %s",
			output.Status, strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	return nil
}

func readNativeClipboard() (ClipboardContent, error) {
	return withClipboard(func(clipboard *systemClipboard) (ClipboardContent, error) {
		return readNativeContent(clipboard, false)
	})
}

func readNativeContent(clipboard nativeClipboard, textOnly bool) (ClipboardContent, error) {
	// Copied files come first: Finder also offers their icon as an image.
	if paths, err := clipboard.getFileList(); err == nil && len(paths) > 0 {
		return ClipboardContentText{Text: fileListText(paths)}, nil
	}
	// The native reader returns raw RGBA pixels; the shared optimizer chooses
	// the ACP encoding.
	if !textOnly {
		if image, err := clipboard.getImage(); err == nil {
			encoded, err := encodeNativeImage(image)
			if err != nil {
				return nil, err
			}
			return ClipboardContentImage{Image: encoded}, nil
		}
	}
	text, err := clipboard.getText()
	if err != nil {
		return nil, fmt.Errorf("read text from system clipboard: %w", err)
	}
	if !textOnly && text == "" {
		return nil, errors.New("clipboard contains neither an image nor text")
	}
	return ClipboardContentText{Text: text}, nil
}

// fileListText is copied files as pasted text: one path per line, quoted
// when it holds whitespace, which the composer reads back as attachable
// paths.
func fileListText(paths []string) string {
	lines := make([]string, 0, len(paths))
	for _, path := range paths {
		path = strings.ToValidUTF8(path, "�")
		if strings.ContainsFunc(path, unicode.IsSpace) {
			lines = append(lines, `"`+path+`"`)
		} else {
			lines = append(lines, path)
		}
	}
	return strings.Join(lines, "\n")
}

func encodeNativeImage(image clipboardImageData) (ClipboardImage, error) {
	if image.width < 0 || uint64(image.width) > math.MaxUint32 {
		return ClipboardImage{}, errors.New("clipboard image width is too large")
	}
	if image.height < 0 || uint64(image.height) > math.MaxUint32 {
		return ClipboardImage{}, errors.New("clipboard image height is too large")
	}
	hi, pixels := bits.Mul64(uint64(image.width), uint64(image.height))
	hi2, expected := bits.Mul64(pixels, 4)
	if hi != 0 || hi2 != 0 || expected > math.MaxInt {
		return ClipboardImage{}, errors.New("clipboard image dimensions overflow")
	}
	if expected > maxDecodedImageBytes {
		return ClipboardImage{}, errors.New("clipboard image expands beyond the supported pixel budget")
	}
	if uint64(len(image.bytes)) != expected {
		return ClipboardImage{}, errors.New("clipboard image has invalid RGBA data")
	}
	optimized, err := client.OptimizeRGBA(uint32(image.width), uint32(image.height), image.bytes)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("optimize clipboard image: %w", err)
	}
	return ClipboardImage{
		DataBase64: base64.StdEncoding.EncodeToString(optimized.Bytes),
		MimeType:   optimized.MimeType,
	}, nil
}

// runningUnderWSL reports a Linux process running under WSL; false on every
// other platform.
func runningUnderWSL() bool {
	if runtime.GOOS != "linux" {
		return false
	}
	if _, ok := os.LookupEnv("WSL_INTEROP"); ok {
		return true
	}
	release, err := os.ReadFile("/proc/sys/kernel/osrelease")
	if err != nil || !utf8.Valid(release) {
		return false
	}
	lowered := textASCIILower(string(release))
	return strings.Contains(lowered, "microsoft") || strings.Contains(lowered, "wsl")
}

func readWSLClipboard() (ClipboardContent, error) {
	return readWSLClipboardWithScript(wslClipboardScript)
}

func readWSLClipboardWithScript(script string) (ClipboardContent, error) {
	executable, ok := wslPowershellExecutable()
	if !ok {
		return nil, errors.New("Windows PowerShell is unavailable; cannot read the WSL clipboard")
	}
	command := core.NewCommandSpec(executable,
		[]string{"-NoProfile", "-NonInteractive", "-Sta", "-Command", composeWSLScript(script)}).
		WithPurpose("read Windows clipboard image or text")
	output, err := core.CancellableProcessExecutorWithTimeout(wslClipboardTimeout).Execute(&command)
	if err != nil {
		return nil, fmt.Errorf("read clipboard through Windows PowerShell: %w", err)
	}
	if output.Status != 0 {
		detail := strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�"))
		if detail == "" {
			return nil, fmt.Errorf("Windows clipboard helper exited with status %d", output.Status)
		}
		if strings.Contains(detail, "ExternalException") {
			return nil, errors.New("Windows clipboard is busy or locked; try again")
		}
		return nil, fmt.Errorf("Windows clipboard helper failed: %s", detail)
	}
	content, err := ParseWSLClipboardOutput(output.Stdout)
	if err != nil {
		return nil, err
	}
	if image, ok := content.(ClipboardContentImage); ok {
		normalized, err := NormalizeImage(image.Image)
		if err != nil {
			return nil, err
		}
		return ClipboardContentImage{Image: normalized}, nil
	}
	return content, nil
}

// NormalizeImage normalizes an encoded clipboard image through the
// controller's shared image optimizer. The PowerShell WSL helper already
// applies a conservative resize, but running the common optimizer here keeps
// its MIME type, dimensions, and byte bound identical to native clipboard
// images.
func NormalizeImage(image ClipboardImage) (ClipboardImage, error) {
	if image.Reference != nil {
		return image, nil
	}
	bytes, err := decodeBase64(image.DataBase64)
	if err != nil {
		return ClipboardImage{}, err
	}
	optimized, err := client.OptimizeImage(bytes)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("optimize clipboard image: %w", err)
	}
	return ClipboardImage{
		DataBase64: base64.StdEncoding.EncodeToString(optimized.Bytes),
		MimeType:   optimized.MimeType,
	}, nil
}

func composeWSLScript(script string) string {
	return fmt.Sprintf("$maxImageBytes = %d;\n$maxDecodedImageBytes = %d;\n%s\n%s",
		MaxImageBytes, maxDecodedImageBytes, wslImageEncoderScript, script)
}

func wslPowershellExecutable() (string, bool) {
	// Prefer the mounted Windows path. The bare name remains useful when a
	// distribution has imported the Windows PATH but mounted drives differ.
	for _, candidate := range []string{
		"/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
		"/mnt/c/Windows/SysNative/WindowsPowerShell/v1.0/powershell.exe",
	} {
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() {
			return candidate, true
		}
	}
	return windowsPowershell, true
}

// ParseWSLClipboardOutput decodes the line-oriented output of the WSL
// helper. Kept public for focused behavioral tests without touching a host
// clipboard.
func ParseWSLClipboardOutput(output []byte) (ClipboardContent, error) {
	if !utf8.Valid(output) {
		return nil, fmt.Errorf("clipboard helper output was not UTF-8: %s", utf8ErrorDescription(output))
	}
	kind, payload, found := strings.Cut(string(output), "\n")
	if !found {
		return nil, errors.New("Windows clipboard helper returned an unsupported format")
	}
	switch strings.TrimRight(kind, "\r") {
	case "IMAGE":
		image, err := ClipboardImageFromPNGBase64(strings.TrimRight(payload, "\r\n"))
		if err != nil {
			return nil, err
		}
		return ClipboardContentImage{Image: image}, nil
	case "TEXT":
		return ClipboardContentText{Text: payload}, nil
	case "EMPTY":
		if payload == "" {
			return nil, errors.New("clipboard contains neither an image nor text")
		}
	}
	return nil, errors.New("Windows clipboard helper returned an unsupported format")
}

// utf8ErrorDescription is Rust's FromUtf8Error Display for invalid output.
func utf8ErrorDescription(output []byte) string {
	index := 0
	for index < len(output) {
		character, size := utf8.DecodeRune(output[index:])
		if character == utf8.RuneError && size <= 1 {
			if !utf8.FullRune(output[index:]) {
				return fmt.Sprintf("incomplete utf-8 byte sequence from index %d", index)
			}
			return fmt.Sprintf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		index += size
	}
	return "invalid utf-8"
}

func decodeBase64(encoded string) ([]byte, error) {
	if encoded == "" {
		return nil, errors.New("clipboard image payload is empty")
	}
	if len(encoded) > maxImageBase64Bytes {
		return nil, fmt.Errorf("clipboard image is too large (maximum %d bytes)", MaxImageBytes)
	}
	// Go's decoder skips line breaks; the Rust engine rejects them.
	if index := strings.IndexAny(encoded, "\r\n"); index >= 0 {
		return nil, fmt.Errorf("decode clipboard image: Invalid symbol %d, offset %d.", encoded[index], index)
	}
	bytes, err := base64.StdEncoding.Strict().DecodeString(encoded)
	if err != nil {
		return nil, fmt.Errorf("decode clipboard image: %w", err)
	}
	if len(bytes) > MaxImageBytes {
		return nil, fmt.Errorf("clipboard image is too large (maximum %d bytes)", MaxImageBytes)
	}
	return bytes, nil
}

func validatePNG(data []byte) error {
	const signature = "\x89PNG\r\n\x1a\n"
	if !strings.HasPrefix(string(data), signature) {
		return errors.New("clipboard image is not a PNG")
	}
	if _, err := png.DecodeConfig(bytes.NewReader(data)); err != nil {
		return fmt.Errorf("decode clipboard PNG header: %w", err)
	}
	// DecodeConfig accepted the IHDR chunk that follows the signature; read
	// its depth and color type to size the decoded rows as the png crate
	// does without transformations.
	header := data[len(signature):]
	if len(header) < 8+13 || string(header[4:8]) != "IHDR" {
		return errors.New("decode clipboard PNG header: missing IHDR chunk")
	}
	width := uint64(binary.BigEndian.Uint32(header[8:12]))
	height := uint64(binary.BigEndian.Uint32(header[12:16]))
	depth := uint64(header[16])
	var samples uint64
	switch header[17] {
	case 0, 3:
		samples = 1
	case 4:
		samples = 2
	case 2:
		samples = 3
	case 6:
		samples = 4
	default:
		return errors.New("clipboard PNG has invalid dimensions")
	}
	rowBytes := (width*samples*depth + 7) / 8
	hi, outputSize := bits.Mul64(rowBytes, height)
	if hi != 0 || outputSize > math.MaxInt64 {
		return errors.New("clipboard PNG has invalid dimensions")
	}
	if outputSize > maxDecodedImageBytes {
		return errors.New("clipboard image expands beyond the supported pixel budget")
	}
	return nil
}
