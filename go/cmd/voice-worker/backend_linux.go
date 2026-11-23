// Port of voice-worker/src/backend.rs: the #[cfg(target_os = "linux")]
// PulseAudio input, which loads libpulse-simple at run time (libloading in
// Rust, github.com/ebitengine/purego here).

package main

import (
	"encoding/binary"
	"fmt"
	"os"
	"sync/atomic"

	"github.com/ebitengine/purego"
)

// inputSourcePulse is Rust's InputSource::Pulse.
type inputSourcePulse struct {
	input *pulseInput
}

func (inputSourcePulse) isInputSource() {}

func (s inputSourcePulse) close() { s.input.close() }

// pulseInput owns the goroutine that reads from PulseAudio. The Pulse read
// may be blocked in a foreign library call, so the goroutine is detached on
// teardown (Rust keeps a JoinHandle it never joins); the worker process
// remains isolated from it and exits promptly when its supervisor cancels
// the worker.
type pulseInput struct {
	stop *atomic.Bool
}

// close is Rust's Drop for PulseInput.
func (p *pulseInput) close() {
	p.stop.Store(true)
}

// pulseSampleSpec is pa_sample_spec, laid out as C lays it out.
type pulseSampleSpec struct {
	format   int32
	rate     uint32
	channels uint8
}

// pulseSimpleNew is pa_simple_new. A nil *byte is a NULL char*; the
// returned pa_simple* is opaque C memory.
type pulseSimpleNew func(server, name *byte, dir int32, dev, streamName *byte, spec *pulseSampleSpec, channelMap, attr uintptr, errorCode *int32) uintptr

// pulseSimpleRead is pa_simple_read.
type pulseSimpleRead func(simple uintptr, data *byte, bytes uintptr, errorCode *int32) int32

// pulseSimpleFree is pa_simple_free.
type pulseSimpleFree func(simple uintptr)

// pulseStrError is pa_strerror; purego copies the returned char* into a Go
// string.
type pulseStrError func(errorCode int32) string

// pulseLibraryMode is the dlopen mode libloading's Library::new uses on
// Unix.
const pulseLibraryMode = purego.RTLD_LAZY | purego.RTLD_LOCAL

const paStreamRecord int32 = 2

// paSampleS16NE is PA_SAMPLE_S16NE: PA_SAMPLE_S16LE (3) on little-endian
// machines, PA_SAMPLE_S16BE (4) on big-endian ones.
var paSampleS16NE = func() int32 {
	if binary.NativeEndian.Uint16([]byte{1, 0}) == 1 {
		return 3
	}
	return 4
}()

// buildPulseSource is build_audio_source's Linux branch: with PULSE_SERVER
// set, the PulseAudio input at the upload sample rate (ok true, also when it
// fails); otherwise ok false and the CPAL path decides.
func buildPulseSource(tx audioSender) (source inputSource, rate uint32, ok bool, err error) {
	if _, set := os.LookupEnv("PULSE_SERVER"); !set {
		return nil, 0, false, nil
	}
	input, err := buildPulseInput(tx)
	if err != nil {
		captureBackendError(fmt.Sprintf("PulseAudio backend unavailable: %v", err))
		return nil, 0, true, err
	}
	return inputSourcePulse{input: input}, sampleRate, true, nil
}

func buildPulseInput(tx audioSender) (*pulseInput, error) {
	panic("unported: voice-worker/src/backend.rs build_pulse_input")
}

// pulseCaptureLoop reads 20 ms chunks from the PulseAudio server (nil for
// the default) until stop is set. server holds no NUL byte.
func pulseCaptureLoop(server *string, stop *atomic.Bool, tx audioSender) error {
	panic("unported: voice-worker/src/backend.rs pulse_capture_loop")
}
