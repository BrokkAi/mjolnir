// Tests of mj-chat/src/clipboard.rs.

package chat

import (
	"bytes"
	"encoding/base64"
	"image"
	"image/png"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// fakeClipboard holds one image and one text, each taken by its first read,
// as Rust's Option::take does.
type fakeClipboard struct {
	image *clipboardImageData
	text  *string
}

func (c *fakeClipboard) getImage() (clipboardImageData, error) {
	if c.image == nil {
		return clipboardImageData{}, errClipboardContentNotAvailable
	}
	image := *c.image
	c.image = nil
	return image, nil
}

func (c *fakeClipboard) getText() (string, error) {
	if c.text == nil {
		return "", errClipboardContentNotAvailable
	}
	text := *c.text
	c.text = nil
	return text, nil
}

func (c *fakeClipboard) getFileList() ([]string, error) {
	return nil, errClipboardContentNotAvailable
}

func clipboardWithBothFormats() *fakeClipboard {
	text := "image caption"
	return &fakeClipboard{
		image: &clipboardImageData{width: 1, height: 1, bytes: []byte{255, 0, 0, 255}},
		text:  &text,
	}
}

func TestNativeTextOnlyClipboardAndMissingTextKeepTheirMeaning(t *testing.T) {
	text := "ordinary text"
	clipboard := &fakeClipboard{text: &text}
	content, err := readNativeContent(clipboard, false)
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := content.(ClipboardContentText); !ok || got.Text != "ordinary text" {
		t.Fatalf("content = %#v", content)
	}

	clipboard = clipboardWithBothFormats()
	clipboard.text = nil
	_, err = readNativeContent(clipboard, true)
	if err == nil || !strings.Contains(err.Error(), "read text from system clipboard") {
		t.Fatalf("error = %v", err)
	}
	if clipboard.image == nil {
		t.Fatal("a text-only read took the image")
	}
}

func TestInvalidNativeImageIsReportedWithoutSilentlySelectingText(t *testing.T) {
	clipboard := clipboardWithBothFormats()
	clipboard.image.bytes = nil
	if _, err := readNativeContent(clipboard, false); err == nil {
		t.Fatal("an invalid image was accepted")
	}
	if clipboard.text == nil || *clipboard.text != "image caption" {
		t.Fatal("the text was read after the image failed")
	}
}

func TestWindowsClipboardWriterStreamsLargeUnicodeTextAndDrainsOutput(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("the WSL clipboard writer exists on Linux only")
	}
	directory := t.TempDir()
	captured := filepath.Join(directory, "stdin")
	text := strings.Repeat("\ufeff日本語 🦀 café\r\nsecond line\n$(`literal`)\n", 4096)
	command := core.NewCommandSpec("sh", []string{
		"-c",
		`head -c 131072 /dev/zero; cat > "$1"`,
		"clipboard-test",
		captured,
	})
	if err := writeWSLClipboardWithCommand(text, command); err != nil {
		t.Fatal(err)
	}
	written, err := os.ReadFile(captured)
	if err != nil {
		t.Fatal(err)
	}
	if len(written) <= 64*1024 {
		t.Fatalf("only %d bytes reached the writer", len(written))
	}
	decoded, err := base64.StdEncoding.DecodeString(string(written))
	if err != nil {
		t.Fatal(err)
	}
	if string(decoded) != text {
		t.Fatal("the writer received different text")
	}
}

func TestWindowsClipboardWriterReportsHelperFailure(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("the WSL clipboard writer exists on Linux only")
	}
	command := core.NewCommandSpec("sh", []string{"-c", "cat >/dev/null; echo 'clipboard denied' >&2; exit 7"})
	err := writeWSLClipboardWithCommand("text", command)
	if err == nil {
		t.Fatal("a failing writer succeeded")
	}
	detail := err.Error()
	if !strings.Contains(detail, "status 7") || !strings.Contains(detail, "clipboard denied") {
		t.Fatal(detail)
	}
}

func TestWindowsClipboardRoundTripPreservesLargeUnicodeText(t *testing.T) {
	t.Skip("requires WSL and temporarily replaces Windows clipboard text")
	if !runningUnderWSL() {
		t.Fatal("not running under WSL")
	}
	// Refuse to replace a non-text clipboard; restore the original text
	// before checking the result, including when the write/read fails.
	content, err := readWSLClipboard()
	if err != nil {
		t.Fatal(err)
	}
	original, ok := content.(ClipboardContentText)
	if !ok {
		t.Fatal("round trip requires a text clipboard to restore")
	}
	text := strings.Repeat("\ufeff日本語 🦀 café\r\nsecond line\n$(`literal`)\n", 4096)
	err = WriteText(text)
	var received string
	if err == nil {
		received, err = ReadText()
	}
	if restoreErr := writeWSLClipboard(original.Text); restoreErr != nil {
		t.Fatalf("restore Windows clipboard text: %v", restoreErr)
	}
	if err != nil {
		t.Fatal(err)
	}
	if received != text {
		t.Fatal("Windows clipboard changed the copied text")
	}
}

func TestDecodesPNGPayloadLargerThanAPipeBuffer(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses cat")
	}
	encodedPNG := noisyPNG(t)
	if len(encodedPNG) <= 64*1024 {
		t.Fatalf("fixture PNG is only %d bytes", len(encodedPNG))
	}
	encoded := base64.StdEncoding.EncodeToString(encodedPNG)
	fixture := filepath.Join(t.TempDir(), "output")
	helperOutput := append([]byte("IMAGE\n"), encoded...)
	if err := os.WriteFile(fixture, helperOutput, 0o600); err != nil {
		t.Fatalf("write clipboard helper fixture: %v", err)
	}
	command := core.NewCommandSpec("cat", []string{fixture}).WithPurpose("test large clipboard image helper")
	output, err := core.CancellableProcessExecutorWithTimeout(3 * time.Second).Execute(&command)
	if err != nil {
		t.Fatalf("large clipboard helper should finish: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("helper status %d", output.Status)
	}
	content, err := ParseWSLClipboardOutput(output.Stdout)
	if err != nil {
		t.Fatalf("large image payload should decode: %v", err)
	}
	imageContent, ok := content.(ClipboardContentImage)
	if !ok {
		t.Fatalf("expected image clipboard content, got %#v", content)
	}
	decoded, err := base64.StdEncoding.DecodeString(imageContent.Image.DataBase64)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(decoded, encodedPNG) {
		t.Fatal("decoded image differs from the helper's PNG")
	}
}

func noisyPNG(t *testing.T) []byte {
	t.Helper()
	const width, height = 256, 256
	pixels := image.NewNRGBA(image.Rect(0, 0, width, height))
	state := uint32(0x1234_5678)
	for index := range pixels.Pix {
		state = state*1_664_525 + 1_013_904_223
		pixels.Pix[index] = byte(state >> 24)
	}
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, pixels); err != nil {
		t.Fatal(err)
	}
	return encoded.Bytes()
}

func TestDownsizesOversizedSyntheticImageInPowershell(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("the WSL clipboard helper exists on Linux only")
	}
	executable, ok := wslPowershellExecutable()
	if !ok {
		return
	}
	if info, err := os.Stat(executable); err != nil || !info.Mode().IsRegular() {
		// The fallback executable name can only be checked from a WSL
		// environment, where the Windows PATH is available to tests.
		return
	}
	script := composeWSLScript(`
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
    [byte[]]$pngBytes = Convert-ImageToPngBytes ` + "`" + `
        -Image $bitmap ` + "`" + `
        -MaxImageBytes $maxImageBytes ` + "`" + `
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
`)
	command := core.NewCommandSpec(executable, []string{"-NoProfile", "-NonInteractive", "-Sta", "-Command", script}).
		WithPurpose("test Windows clipboard image resizing")
	output, err := core.CancellableProcessExecutorWithTimeout(wslClipboardTimeout).Execute(&command)
	if err != nil {
		t.Fatalf("PowerShell image resize fixture should finish: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("PowerShell image resize fixture failed: %s", output.Stderr)
	}
	var fields []int
	for _, field := range strings.Split(string(output.Stdout), "|") {
		value, err := strconv.Atoi(field)
		if err != nil {
			t.Fatalf("PowerShell fixture field should be numeric: %q", field)
		}
		fields = append(fields, value)
	}
	if len(fields) != 5 {
		t.Fatalf("fields = %v", fields)
	}
	if fields[0] > MaxImageBytes {
		t.Fatalf("PNG of %d bytes exceeds the budget", fields[0])
	}
	if fields[1] != 768 || fields[2] != 768 {
		t.Fatalf("source dimensions %dx%d", fields[1], fields[2])
	}
	if !(fields[3] < fields[1] || fields[4] < fields[2]) {
		t.Fatalf("image was not reduced: %v", fields)
	}
}
