// Port of mj-client/src/image.rs.

// Decode and constrain user supplied images for controller and client APIs.
//
// Rust's `image` crate becomes Go's image, image/jpeg and image/png, with
// golang.org/x/image/webp for WebP and golang.org/x/image/draw for resizing.
// A DynamicImage is an image.Image carried with the bytes per pixel of the
// representation the `image` crate decodes it to (its ColorType's
// bytes_per_pixel), which every size check reads.

package client

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"math"

	"golang.org/x/image/draw"
	"golang.org/x/image/webp"

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

// imageFormat is the format the `image` crate guesses from the first bytes.
type imageFormat int

const (
	imageFormatUnknown imageFormat = iota
	imageFormatPNG
	imageFormatJPEG
	imageFormatWebP
	// imageFormatOther is a format the `image` crate recognizes but this
	// module does not accept.
	imageFormatOther
)

func (f imageFormat) mimeType() string {
	switch f {
	case imageFormatPNG:
		return "image/png"
	case imageFormatJPEG:
		return "image/jpeg"
	case imageFormatWebP:
		return "image/webp"
	}
	return "application/octet-stream"
}

// guessImageFormat is ImageReader::with_guessed_format: the format the
// magic bytes name, among those the `image` crate knows.
func guessImageFormat(data []byte) imageFormat {
	has := func(prefix string) bool { return bytes.HasPrefix(data, []byte(prefix)) }
	switch {
	case has("\x89PNG\r\n\x1a\n"):
		return imageFormatPNG
	case has("\xff\xd8\xff"):
		return imageFormatJPEG
	case len(data) >= 12 && has("RIFF") && string(data[8:12]) == "WEBP":
		return imageFormatWebP
	case has("GIF87a"), has("GIF89a"), has("BM"), has("II*\x00"), has("MM\x00*"),
		has("\x00\x00\x01\x00"), has("#?RADIANCE"), has("v/1\x01"), has("DDS "),
		has("farbfeld"), has("qoif"), has("8BPS"):
		return imageFormatOther
	case len(data) >= 2 && data[0] == 'P' && data[1] >= '1' && data[1] <= '7':
		return imageFormatOther
	case len(data) >= 12 && string(data[4:8]) == "ftyp" && (string(data[8:12]) == "avif" || string(data[8:12]) == "avis"):
		return imageFormatOther
	}
	return imageFormatUnknown
}

// OptimizeImage decodes and optimizes a JPEG, PNG, or WebP image.
//
// Images larger than the output budget are recompressed and progressively
// resized. EXIF orientation is applied before deciding the output
// dimensions.
func OptimizeImage(data []byte) (OptimizedImage, error) {
	if len(data) > maxInputBytes {
		return OptimizedImage{}, errors.New("image input exceeds the 64 MiB limit")
	}
	format := guessImageFormat(data)
	switch format {
	case imageFormatUnknown:
		return OptimizedImage{}, errors.New("unsupported image format")
	case imageFormatOther:
		return OptimizedImage{}, errors.New("unsupported image format (expected JPEG, PNG, or WebP)")
	}

	var config image.Config
	var err error
	switch format {
	case imageFormatPNG:
		config, err = png.DecodeConfig(bytes.NewReader(data))
	case imageFormatJPEG:
		config, err = jpeg.DecodeConfig(bytes.NewReader(data))
	default:
		config, err = webp.DecodeConfig(bytes.NewReader(data))
	}
	if err != nil {
		return OptimizedImage{}, fmt.Errorf("create image decoder: %w", err)
	}
	orientation := imageOrientation(format, data)
	width, height := uint32(config.Width), uint32(config.Height)
	bytesPerPixel := decodedBytesPerPixel(format, config.ColorModel, data)
	// The decoder's total_bytes saturates, so an overflow is too large too.
	if _, err := checkedImageBytes(width, height, bytesPerPixel); err != nil {
		return OptimizedImage{}, errors.New("decoded image exceeds the 256 MiB limit")
	}

	var decoded image.Image
	switch format {
	case imageFormatPNG:
		decoded, err = png.Decode(bytes.NewReader(data))
	case imageFormatJPEG:
		decoded, err = jpeg.Decode(bytes.NewReader(data))
	default:
		decoded, err = webp.Decode(bytes.NewReader(data))
	}
	if err != nil {
		return OptimizedImage{}, fmt.Errorf("decode image: %w", err)
	}
	img := dynamicImage{image: normalizeDecoded(decoded), bytesPerPixel: bytesPerPixel}
	img = img.applyOrientation(orientation)

	// A normal JPEG or PNG can keep its original metadata and compression
	// when it already fits.
	if orientation == orientationNoTransforms && (format == imageFormatJPEG || format == imageFormatPNG) &&
		len(data) <= maxOutputBytes {
		cloned, err := cloneBytes(data)
		if err != nil {
			return OptimizedImage{}, err
		}
		bounds := img.image.Bounds()
		return OptimizedImage{
			Bytes:    cloned,
			MimeType: format.mimeType(),
			Width:    uint32(bounds.Dx()),
			Height:   uint32(bounds.Dy()),
		}, nil
	}

	// Keep this check tied to the decoder's advertised representation. It
	// catches dimensions whose multiplication would overflow before any
	// image buffer is allocated.
	if _, err := checkedImageBytes(width, height, img.bytesPerPixel); err != nil {
		return OptimizedImage{}, err
	}
	return optimizeDynamicImage(img)
}

// OptimizeRGBA optimizes an 8-bit RGBA image supplied as raw pixels.
func OptimizeRGBA(width, height uint32, rgba []byte) (OptimizedImage, error) {
	if width == 0 || height == 0 {
		return OptimizedImage{}, errors.New("image dimensions must be non-zero")
	}
	expected, err := checkedRGBALen(width, height)
	if err != nil {
		return OptimizedImage{}, err
	}
	if len(rgba) != expected {
		return OptimizedImage{}, errors.New("RGBA buffer length does not match image dimensions")
	}
	pixels := make([]byte, len(rgba))
	copy(pixels, rgba)
	img := &image.NRGBA{
		Pix:    pixels,
		Stride: int(width) * 4,
		Rect:   image.Rect(0, 0, int(width), int(height)),
	}
	return optimizeDynamicImage(dynamicImage{image: img, bytesPerPixel: 4})
}

// dynamicImage is a decoded image with the bytes per pixel of the `image`
// crate's representation of it.
type dynamicImage struct {
	image         image.Image
	bytesPerPixel uint8
}

func (d dynamicImage) dimensions() (uint32, uint32) {
	bounds := d.image.Bounds()
	return uint32(bounds.Dx()), uint32(bounds.Dy())
}

func optimizeDynamicImage(img dynamicImage) (OptimizedImage, error) {
	transparent := containsTransparency(img.image)
	for {
		width, height := img.dimensions()

		// JPEG dimensions are limited to a 16-bit unsigned value. Resize
		// before asking the encoder to handle an otherwise valid PNG/WebP
		// with larger dimensions.
		if !transparent && (width > maxJPEGDimension || height > maxJPEGDimension) {
			nextWidth, nextHeight := fitDimensions(width, height, maxJPEGDimension, maxJPEGDimension)
			resized, err := resizeChecked(img, nextWidth, nextHeight)
			if err != nil {
				return OptimizedImage{}, err
			}
			img = resized
			continue
		}

		if transparent {
			encoded, fits, err := encodePNG(img.image)
			if err != nil {
				return OptimizedImage{}, err
			}
			if fits {
				return OptimizedImage{Bytes: encoded, MimeType: "image/png", Width: width, Height: height}, nil
			}
		} else {
			for _, quality := range jpegQualities {
				encoded, fits, err := encodeJPEG(img.image, quality)
				if err != nil {
					return OptimizedImage{}, err
				}
				if fits {
					return OptimizedImage{Bytes: encoded, MimeType: "image/jpeg", Width: width, Height: height}, nil
				}
			}
		}

		nextWidth, nextHeight := reducedDimensions(width, height)
		if nextWidth == width && nextHeight == height {
			return OptimizedImage{}, errors.New("could not encode image within the 700 KiB output limit")
		}
		resized, err := resizeChecked(img, nextWidth, nextHeight)
		if err != nil {
			return OptimizedImage{}, err
		}
		img = resized
	}
}

// encodePNG is the PNG encoding of img, or false when it does not fit the
// output budget.
func encodePNG(img image.Image) ([]byte, bool, error) {
	writer := newLimitedWriter(maxOutputBytes)
	encoder := png.Encoder{CompressionLevel: png.BestCompression}
	err := encoder.Encode(writer, img)
	if writer.tooLarge {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("encode PNG: %w", err)
	}
	return writer.intoInner(), true, nil
}

// encodeJPEG is the JPEG encoding of img at quality, or false when it does
// not fit the output budget. The encoder ignores alpha, which an opaque image
// does not use.
func encodeJPEG(img image.Image, quality uint8) ([]byte, bool, error) {
	writer := newLimitedWriter(maxOutputBytes)
	err := jpeg.Encode(writer, img, &jpeg.Options{Quality: int(quality)})
	if writer.tooLarge {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("encode JPEG: %w", err)
	}
	return writer.intoInner(), true, nil
}

func containsTransparency(img image.Image) bool {
	switch img := img.(type) {
	case *image.NRGBA:
		for i := 3; i < len(img.Pix); i += 4 {
			if img.Pix[i] != math.MaxUint8 {
				return true
			}
		}
	case *image.RGBA:
		for i := 3; i < len(img.Pix); i += 4 {
			if img.Pix[i] != math.MaxUint8 {
				return true
			}
		}
	case *image.NRGBA64:
		for i := 6; i < len(img.Pix); i += 8 {
			if img.Pix[i] != math.MaxUint8 || img.Pix[i+1] != math.MaxUint8 {
				return true
			}
		}
	case *image.RGBA64:
		for i := 6; i < len(img.Pix); i += 8 {
			if img.Pix[i] != math.MaxUint8 || img.Pix[i+1] != math.MaxUint8 {
				return true
			}
		}
	case *image.NYCbCrA:
		for _, alpha := range img.A {
			if alpha != math.MaxUint8 {
				return true
			}
		}
	}
	return false
}

func resizeChecked(img dynamicImage, width, height uint32) (dynamicImage, error) {
	if width == 0 || height == 0 {
		return dynamicImage{}, errors.New("image dimensions must be non-zero")
	}
	currentWidth, currentHeight := img.dimensions()
	currentBytes, err := checkedImageBytes(currentWidth, currentHeight, img.bytesPerPixel)
	if err != nil {
		return dynamicImage{}, err
	}
	nextBytes, err := checkedImageBytes(width, height, img.bytesPerPixel)
	if err != nil {
		return dynamicImage{}, err
	}
	if currentBytes > math.MaxUint64-nextBytes || currentBytes+nextBytes > maxDecodedBytes*2 {
		return dynamicImage{}, errors.New("image resize would exceed the memory limit")
	}
	resized := newImageLike(img.image, int(width), int(height))
	lanczos3.Scale(resized, resized.Bounds(), img.image, img.image.Bounds(), draw.Src, nil)
	return dynamicImage{image: resized, bytesPerPixel: img.bytesPerPixel}, nil
}

// lanczos3 is the `image` crate's FilterType::Lanczos3: a windowed sinc with
// a support of three.
var lanczos3 = &draw.Kernel{Support: 3, At: func(t float64) float64 {
	if t <= -3 || t >= 3 {
		return 0
	}
	return lanczosSinc(t) * lanczosSinc(t/3)
}}

func lanczosSinc(t float64) float64 {
	if t == 0 {
		return 1
	}
	a := t * math.Pi
	return math.Sin(a) / a
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

// errImageDimensionsOverflow is the overflow failure of the size checks.
var errImageDimensionsOverflow = errors.New("image dimensions overflow")

func checkedRGBALen(width, height uint32) (int, error) {
	pixels := uint64(width) * uint64(height)
	if pixels > math.MaxUint64/4 {
		return 0, errors.New("RGBA image dimensions overflow")
	}
	size := pixels * 4
	if size > maxDecodedBytes {
		return 0, errors.New("decoded image exceeds the 256 MiB limit")
	}
	if size > math.MaxInt {
		return 0, errors.New("RGBA image is too large for this platform")
	}
	return int(size), nil
}

// checkedImageBytes is the decoded size of a width by height image whose
// pixels take bytesPerPixel bytes each (Rust passes the ColorType).
func checkedImageBytes(width, height uint32, bytesPerPixel uint8) (uint64, error) {
	pixels := uint64(width) * uint64(height)
	if bytesPerPixel != 0 && pixels > math.MaxUint64/uint64(bytesPerPixel) {
		return 0, errImageDimensionsOverflow
	}
	size := pixels * uint64(bytesPerPixel)
	if size > maxDecodedBytes {
		return 0, errors.New("decoded image exceeds the 256 MiB limit")
	}
	return size, nil
}

// decodedBytesPerPixel is Rust's ColorType::bytes_per_pixel for the
// representation the `image` crate decodes data to: what its decoder
// advertises before decoding.
func decodedBytesPerPixel(format imageFormat, model color.Model, data []byte) uint8 {
	switch format {
	case imageFormatJPEG:
		if model == color.GrayModel {
			return 1
		}
		// YCbCr and CMYK both decode to RGB.
		return 3
	case imageFormatWebP:
		if model == color.NYCbCrAModel || model == color.NRGBAModel {
			return 4
		}
		return 3
	}
	switch model {
	case color.GrayModel:
		return 1
	case color.Gray16Model:
		return 2
	case color.RGBAModel:
		return 3
	case color.NRGBAModel:
		if pngColorType(data) == 4 {
			// Gray with alpha.
			return 2
		}
		return 4
	case color.RGBA64Model:
		return 6
	case color.NRGBA64Model:
		if pngColorType(data) == 4 {
			return 4
		}
		return 8
	}
	if palette, ok := model.(color.Palette); ok {
		for _, entry := range palette {
			if _, _, _, alpha := entry.RGBA(); alpha != 0xffff {
				return 4
			}
		}
		return 3
	}
	return 4
}

// pngColorType is the IHDR color type of a PNG, or 255 when it cannot be
// read.
func pngColorType(data []byte) byte {
	// Signature (8), chunk length (4), "IHDR" (4), width, height (8), bit
	// depth (1), then the color type.
	if len(data) < 26 || string(data[12:16]) != "IHDR" {
		return 255
	}
	return data[25]
}

// normalizeDecoded turns representations the `image` crate expands while
// decoding into the Go type that holds the expanded pixels: a palette into
// its colors and CMYK into RGB.
func normalizeDecoded(img image.Image) image.Image {
	switch img.(type) {
	case *image.Paletted, *image.CMYK:
		bounds := img.Bounds()
		expanded := image.NewNRGBA(image.Rect(0, 0, bounds.Dx(), bounds.Dy()))
		draw.Draw(expanded, expanded.Bounds(), img, bounds.Min, draw.Src)
		return expanded
	}
	return img
}

// newImageLike is an empty width by height image holding the same kind of
// pixels as img.
func newImageLike(img image.Image, width, height int) draw.Image {
	bounds := image.Rect(0, 0, width, height)
	switch img.(type) {
	case *image.Gray:
		return image.NewGray(bounds)
	case *image.Gray16:
		return image.NewGray16(bounds)
	case *image.NRGBA64:
		return image.NewNRGBA64(bounds)
	case *image.RGBA64:
		return image.NewRGBA64(bounds)
	case *image.RGBA, *image.YCbCr:
		return image.NewRGBA(bounds)
	}
	return image.NewNRGBA(bounds)
}

// imageOrientationKind is the `image` crate's metadata::Orientation, from the
// EXIF orientation tag.
type imageOrientationKind int

const (
	orientationNoTransforms imageOrientationKind = iota
	orientationRotate90
	orientationRotate180
	orientationRotate270
	orientationFlipHorizontal
	orientationFlipVertical
	orientationRotate90FlipH
	orientationRotate270FlipH
)

// orientationFromExif is Orientation::from_exif.
func orientationFromExif(value uint16) (imageOrientationKind, bool) {
	switch value {
	case 1:
		return orientationNoTransforms, true
	case 2:
		return orientationFlipHorizontal, true
	case 3:
		return orientationRotate180, true
	case 4:
		return orientationFlipVertical, true
	case 5:
		return orientationRotate90FlipH, true
	case 6:
		return orientationRotate90, true
	case 7:
		return orientationRotate270FlipH, true
	case 8:
		return orientationRotate270, true
	}
	return orientationNoTransforms, false
}

// imageOrientation is the decoder's orientation(): the EXIF orientation of
// the image's metadata, or no transforms when it has none.
func imageOrientation(format imageFormat, data []byte) imageOrientationKind {
	var exif []byte
	switch format {
	case imageFormatJPEG:
		exif = jpegExif(data)
	case imageFormatPNG:
		exif = pngExif(data)
	case imageFormatWebP:
		exif = webpExif(data)
	}
	if exif == nil {
		return orientationNoTransforms
	}
	value, ok := exifOrientation(exif)
	if !ok {
		return orientationNoTransforms
	}
	orientation, ok := orientationFromExif(value)
	if !ok {
		return orientationNoTransforms
	}
	return orientation
}

// jpegExif is the TIFF data of a JPEG's first Exif APP1 segment.
func jpegExif(data []byte) []byte {
	position := 2
	for position+4 <= len(data) {
		if data[position] != 0xff {
			return nil
		}
		marker := data[position+1]
		if marker == 0xff {
			position++
			continue
		}
		if marker == 0xd9 || marker == 0xda {
			return nil
		}
		if marker >= 0xd0 && marker <= 0xd7 || marker == 0x01 {
			position += 2
			continue
		}
		length := int(binary.BigEndian.Uint16(data[position+2:]))
		if length < 2 || position+2+length > len(data) {
			return nil
		}
		segment := data[position+4 : position+2+length]
		if marker == 0xe1 && bytes.HasPrefix(segment, []byte("Exif\x00\x00")) {
			return segment[6:]
		}
		position += 2 + length
	}
	return nil
}

// pngExif is the data of a PNG's eXIf chunk.
func pngExif(data []byte) []byte {
	position := 8
	for position+12 <= len(data) {
		length := int(binary.BigEndian.Uint32(data[position:]))
		kind := string(data[position+4 : position+8])
		if length < 0 || position+12+length > len(data) {
			return nil
		}
		if kind == "eXIf" {
			return data[position+8 : position+8+length]
		}
		if kind == "IDAT" || kind == "IEND" {
			return nil
		}
		position += 12 + length
	}
	return nil
}

// webpExif is the data of an extended WebP's EXIF chunk.
func webpExif(data []byte) []byte {
	position := 12
	for position+8 <= len(data) {
		kind := string(data[position : position+4])
		length := int(binary.LittleEndian.Uint32(data[position+4:]))
		if length < 0 || position+8+length > len(data) {
			return nil
		}
		if kind == "EXIF" {
			chunk := data[position+8 : position+8+length]
			// Some writers keep JPEG's Exif header in the chunk.
			return bytes.TrimPrefix(chunk, []byte("Exif\x00\x00"))
		}
		position += 8 + length + length%2
	}
	return nil
}

// exifOrientation reads the orientation tag of IFD0 of TIFF-structured EXIF
// data.
func exifOrientation(tiff []byte) (uint16, bool) {
	if len(tiff) < 8 {
		return 0, false
	}
	var order binary.ByteOrder
	switch string(tiff[:4]) {
	case "II*\x00":
		order = binary.LittleEndian
	case "MM\x00*":
		order = binary.BigEndian
	default:
		return 0, false
	}
	offset := int(order.Uint32(tiff[4:]))
	if offset < 8 || offset+2 > len(tiff) {
		return 0, false
	}
	entries := int(order.Uint16(tiff[offset:]))
	for index := 0; index < entries; index++ {
		entry := offset + 2 + index*12
		if entry+12 > len(tiff) {
			return 0, false
		}
		if order.Uint16(tiff[entry:]) != 0x0112 {
			continue
		}
		if order.Uint16(tiff[entry+2:]) != 3 || order.Uint32(tiff[entry+4:]) != 1 {
			return 0, false
		}
		return order.Uint16(tiff[entry+8:]), true
	}
	return 0, false
}

// applyOrientation is DynamicImage::apply_orientation.
func (d dynamicImage) applyOrientation(orientation imageOrientationKind) dynamicImage {
	if orientation == orientationNoTransforms {
		return d
	}
	bounds := d.image.Bounds()
	width, height := bounds.Dx(), bounds.Dy()
	// Where the source pixel of each output pixel is.
	var outWidth, outHeight int
	var source func(x, y int) (int, int)
	switch orientation {
	case orientationRotate90:
		outWidth, outHeight = height, width
		source = func(x, y int) (int, int) { return y, height - 1 - x }
	case orientationRotate180:
		outWidth, outHeight = width, height
		source = func(x, y int) (int, int) { return width - 1 - x, height - 1 - y }
	case orientationRotate270:
		outWidth, outHeight = height, width
		source = func(x, y int) (int, int) { return width - 1 - y, x }
	case orientationFlipHorizontal:
		outWidth, outHeight = width, height
		source = func(x, y int) (int, int) { return width - 1 - x, y }
	case orientationFlipVertical:
		outWidth, outHeight = width, height
		source = func(x, y int) (int, int) { return x, height - 1 - y }
	case orientationRotate90FlipH:
		// Rotate 90 degrees clockwise, then flip horizontally: a transpose.
		outWidth, outHeight = height, width
		source = func(x, y int) (int, int) { return y, x }
	case orientationRotate270FlipH:
		// Rotate 270 degrees clockwise, then flip horizontally.
		outWidth, outHeight = height, width
		source = func(x, y int) (int, int) { return width - 1 - y, height - 1 - x }
	}
	oriented := newImageLike(d.image, outWidth, outHeight)
	for y := 0; y < outHeight; y++ {
		for x := 0; x < outWidth; x++ {
			sx, sy := source(x, y)
			oriented.Set(x, y, d.image.At(bounds.Min.X+sx, bounds.Min.Y+sy))
		}
	}
	return dynamicImage{image: oriented, bytesPerPixel: d.bytesPerPixel}
}

func cloneBytes(data []byte) ([]byte, error) {
	cloned := make([]byte, len(data))
	copy(cloned, data)
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

func (w *limitedWriter) Write(data []byte) (int, error) {
	if len(w.bytes)+len(data) > w.limit {
		w.tooLarge = true
		return 0, errors.New(errEncodedImageTooBig)
	}
	w.bytes = append(w.bytes, data...)
	return len(data), nil
}
