// Port of the tests in mj-client/src/image.rs.

package client

import (
	"strings"
	"testing"
)

func imageNoise(t *testing.T, width, height uint32) []byte {
	t.Helper()
	size, err := checkedRGBALen(width, height)
	if err != nil {
		t.Fatal(err)
	}
	state := uint32(0x1234_5678)
	pixels := make([]byte, size)
	for i := range pixels {
		state = state*1_664_525 + 1_013_904_223
		pixels[i] = byte(state >> 24)
	}
	for i := 3; i < len(pixels); i += 4 {
		pixels[i] = 0xff
	}
	return pixels
}

func TestOversizedInputIsRejectedBeforeDecoding(t *testing.T) {
	_, err := OptimizeImage(make([]byte, maxInputBytes+1))
	if err == nil || !strings.Contains(err.Error(), "64 MiB") {
		t.Fatalf("error = %v", err)
	}
}

func TestNoisyImageIsResizedToFitOutputBudget(t *testing.T) {
	const width, height = 1_600, 1_200
	optimized, err := OptimizeRGBA(width, height, imageNoise(t, width, height))
	if err != nil {
		t.Fatal(err)
	}
	if optimized.MimeType != "image/jpeg" {
		t.Errorf("mime type %q", optimized.MimeType)
	}
	if len(optimized.Bytes) > maxOutputBytes {
		t.Errorf("%d bytes over the budget", len(optimized.Bytes))
	}
	if !(optimized.Width < width || optimized.Height < height) {
		t.Errorf("not resized: %dx%d", optimized.Width, optimized.Height)
	}
}
