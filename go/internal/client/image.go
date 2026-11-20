// Port of mj-client/src/image.rs.

// Decode and constrain user supplied images for controller and client APIs.
//
// Rust's `image` crate becomes Go's image, image/jpeg and image/png, with
// golang.org/x/image/webp for WebP and golang.org/x/image/draw for resizing.
// A DynamicImage is an image.Image; its ColorType's bytes per pixel is
// imageBytesPerPixel.

package client

import (
	"errors"
	"image"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	maxInputBytes                = 64 * 1024 * 1024
	maxDecodedBytes       uint64 = 256 * 1024 * 1024
	maxOutputBytes               = core.MaxImageBytes
	maxJPEGDimension      uint32 = 65_535
	errEncodedImageTooBig        = "encoded image exceeds size limit"
)

var jpegQualities = [3]uint8{90, 85, 80}

// OptimizedImage is an image encoded in a bounded, browser-friendly
// representation.
type OptimizedImage struct {
	Bytes    []byte
	MimeType string
	Width    uint32
	Height   uint32
}

// OptimizeImage decodes and optimizes a JPEG, PNG, or WebP image.
//
// Images larger than the output budget are recompressed and progressively
// resized. EXIF orientation is applied before deciding the output
// dimensions.
func OptimizeImage(bytes []byte) (OptimizedImage, error) {
	panic("unported: mj-client/src/image.rs optimize_image")
}

// OptimizeRGBA optimizes an 8-bit RGBA image supplied as raw pixels.
func OptimizeRGBA(width, height uint32, rgba []byte) (OptimizedImage, error) {
	panic("unported: mj-client/src/image.rs optimize_rgba")
}

func optimizeDynamicImage(img image.Image) (OptimizedImage, error) {
	panic("unported: mj-client/src/image.rs optimize_dynamic_image")
}

// encodePNG is the PNG encoding of img, or false when it does not fit the
// output budget.
func encodePNG(img image.Image) ([]byte, bool, error) {
	panic("unported: mj-client/src/image.rs encode_png")
}

// encodeJPEG is the JPEG encoding of img at quality, or false when it does
// not fit the output budget.
func encodeJPEG(img image.Image, quality uint8) ([]byte, bool, error) {
	panic("unported: mj-client/src/image.rs encode_jpeg")
}

func containsTransparency(img image.Image) bool {
	panic("unported: mj-client/src/image.rs contains_transparency")
}

func resizeChecked(img image.Image, width, height uint32) (image.Image, error) {
	panic("unported: mj-client/src/image.rs resize_checked")
}

func reducedDimensions(width, height uint32) (uint32, uint32) {
	nextWidth := uint32(max(uint64(width)*4/5, 1))
	nextHeight := uint32(max(uint64(height)*4/5, 1))
	if nextWidth == width && nextHeight == height {
		if width > height {
			return max(width, 1) - 1, height
		}
		return width, max(height, 1) - 1
	}
	return nextWidth, nextHeight
}

func fitDimensions(width, height, maxWidth, maxHeight uint32) (uint32, uint32) {
	if width <= maxWidth && height <= maxHeight {
		return width, height
	}
	widthScale := uint64(maxWidth) * uint64(height)
	heightScale := uint64(maxHeight) * uint64(width)
	numerator, denominator := uint64(maxHeight), uint64(height)
	if widthScale <= heightScale {
		numerator, denominator = uint64(maxWidth), uint64(width)
	}
	nextWidth := max(uint64(width)*numerator/denominator, 1)
	nextHeight := max(uint64(height)*numerator/denominator, 1)
	return uint32(nextWidth), uint32(nextHeight)
}

func checkedRGBALen(width, height uint32) (int, error) {
	panic("unported: mj-client/src/image.rs checked_rgba_len")
}

// checkedImageBytes is the decoded size of a width by height image whose
// pixels take bytesPerPixel bytes each (Rust passes the ColorType).
func checkedImageBytes(width, height uint32, bytesPerPixel uint8) (uint64, error) {
	panic("unported: mj-client/src/image.rs checked_image_bytes")
}

// imageBytesPerPixel is Rust's ColorType::bytes_per_pixel for the pixel
// representation img holds.
func imageBytesPerPixel(img image.Image) uint8 {
	panic("unported: mj-client/src/image.rs ColorType::bytes_per_pixel")
}

func cloneBytes(bytes []byte) ([]byte, error) {
	cloned := make([]byte, len(bytes))
	copy(cloned, bytes)
	return cloned, nil
}

// limitedWriter collects an encoder's output and refuses to grow past limit.
type limitedWriter struct {
	bytes    []byte
	limit    int
	tooLarge bool
}

func newLimitedWriter(limit int) *limitedWriter {
	return &limitedWriter{limit: limit}
}

func (w *limitedWriter) intoInner() []byte { return w.bytes }

func (w *limitedWriter) Write(bytes []byte) (int, error) {
	if len(w.bytes)+len(bytes) > w.limit {
		w.tooLarge = true
		return 0, errors.New(errEncodedImageTooBig)
	}
	w.bytes = append(w.bytes, bytes...)
	return len(bytes), nil
}
