// Port of the tests in voice-worker/src/backend.rs.

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestGoldenEncodeMonoPCM16WAV(t *testing.T) {
	// The golden is the RIFF/WAVE chunk layout with little-endian PCM16 samples.
	output := filepath.Join(t.TempDir(), "capture.wav")
	commands := make(chan stdinCommand)
	defer close(commands)
	outcome, err := saveCapture(output, capturedAudio{
		samples:    []float32{-2.0, -0.5, 0.0, 0.5, 2.0},
		sampleRate: 16_000,
		commandRx:  commands,
	})
	if err != nil {
		t.Fatalf("save captured audio: %v", err)
	}
	if outcome != runOutcomeCaptured {
		t.Fatalf("outcome = %v, want captured", outcome)
	}
	wav, err := os.ReadFile(output)
	if err != nil {
		t.Fatal(err)
	}
	bytes := make([]string, len(wav))
	for index, b := range wav {
		bytes[index] = fmt.Sprintf("%02x", b)
	}
	actual := fmt.Sprintf("=== mono PCM16 WAV (%d bytes) ===\n%s", len(wav), strings.Join(bytes, " "))
	core.AssertGolden(t, "testdata", "mono-pcm16-wav", actual)
}

func TestEncodeWAVRejectsAudioBeyondTheRIFFSizeLimit(t *testing.T) {
	if strconv.IntSize != 64 {
		t.Skip("needs a 64-bit platform (Rust: cfg(target_pointer_width = \"64\"))")
	}
	const sampleCount = 2_147_483_630
	// Go hands a large allocation fresh zeroed pages from the system without
	// touching them, so this reserves virtual memory as Rust's alloc_zeroed
	// does. encodeWAV must reject its length before reading samples or
	// allocating output.
	samples := make([]float32, sampleCount)
	_, err := encodeWAV(samples, 16_000)
	if err == nil {
		t.Fatal("RIFF length cannot represent this audio")
	}
	if got, want := err.Error(), "audio capture exceeds RIFF size limit"; got != want {
		t.Fatalf("error = %q, want %q", got, want)
	}
}

func TestCaptureQueueReportsOverloadWithoutBlockingTheAudioCallback(t *testing.T) {
	rx := make(chan audioMessage, 4)
	state := &audioQueueState{}
	sender := audioSender{tx: rx, state: state}
	for range 5 {
		sender.trySend(audioMessage{samples: slices.Repeat([]float32{0.25}, 4096)})
	}
	if !state.overflowed.Load() {
		t.Fatal("a full queue must report overflow")
	}
	select {
	case message := <-rx:
		if message.err != nil {
			t.Fatalf("unexpected capture error: %v", message.err)
		}
		if !slices.Equal(message.samples, slices.Repeat([]float32{0.25}, 4096)) {
			t.Fatalf("first queued chunk changed: %v", message.samples)
		}
	default:
		t.Fatal("the queue holds the chunks sent before it filled")
	}
}
