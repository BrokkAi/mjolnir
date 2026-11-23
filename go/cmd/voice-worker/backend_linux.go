// Port of voice-worker/src/backend.rs: the #[cfg(target_os = "linux")]
// PulseAudio input, which loads libpulse-simple at run time (libloading in
// Rust, github.com/ebitengine/purego here).

package main

import (
	"encoding/binary"
	"fmt"
	"math"
	"os"
	"runtime"
	"strings"
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
	var server *string
	if value, ok := os.LookupEnv("PULSE_SERVER"); ok {
		if position := strings.IndexByte(value, 0); position >= 0 {
			return nil, fmt.Errorf("PULSE_SERVER contains NUL: nul byte found in provided data at position: %d", position)
		}
		server = &value
	}
	stop := &atomic.Bool{}
	go func() {
		if err := pulseCaptureLoop(server, stop, tx); err != nil && !stop.Load() {
			tx.trySend(audioMessage{err: err})
		}
	}()

	// Opening the connection in the reader goroutine lets the main capture
	// loop remain responsive while a WSLg Pulse server is slow or
	// unavailable.
	return &pulseInput{stop: stop}, nil
}

// pulseCaptureLoop reads 20 ms chunks from the PulseAudio server (nil for
// the default) until stop is set. server holds no NUL byte.
func pulseCaptureLoop(server *string, stop *atomic.Bool, tx audioSender) error {
	library, err := purego.Dlopen("libpulse-simple.so.0", pulseLibraryMode)
	if err != nil {
		return fmt.Errorf("load libpulse-simple.so.0: %w", err)
	}
	// Dropping libloading's Library closes it and ignores dlclose's result.
	defer func() { _ = purego.Dlclose(library) }()
	var simpleNew pulseSimpleNew
	if err := loadPulseFunction(&simpleNew, library, "pa_simple_new"); err != nil {
		return err
	}
	var simpleRead pulseSimpleRead
	if err := loadPulseFunction(&simpleRead, library, "pa_simple_read"); err != nil {
		return err
	}
	var simpleFree pulseSimpleFree
	if err := loadPulseFunction(&simpleFree, library, "pa_simple_free"); err != nil {
		return err
	}
	var strError pulseStrError
	if err := loadPulseFunction(&strError, library, "pa_strerror"); err != nil {
		return err
	}
	application := cString("mj-voice-worker")
	streamName := cString("dictation")
	spec := pulseSampleSpec{
		format:   paSampleS16NE,
		rate:     sampleRate,
		channels: 1,
	}
	serverName := "default"
	var serverC *byte
	if server != nil {
		serverName = strings.ToValidUTF8(*server, "�")
		serverC = cString(*server)
	}
	fmt.Fprintf(os.Stderr, "voice capture: backend=PulseAudio, server=%s, device=default, rate=%d Hz, channels=1, format=PCM16\n", serverName, sampleRate)
	var errorCode int32
	handle := simpleNew(serverC, application, paStreamRecord, nil, streamName, &spec, 0, 0, &errorCode)
	// pa_simple_new copies what it keeps; the arguments only have to live
	// through the call.
	runtime.KeepAlive(serverC)
	runtime.KeepAlive(application)
	runtime.KeepAlive(streamName)
	runtime.KeepAlive(&spec)
	if handle == 0 {
		return fmt.Errorf("pa_simple_new failed with PulseAudio error %d: %s", errorCode, strError(errorCode))
	}

	var bytes [640]byte // 20 ms of 16 kHz mono PCM16.
	for !stop.Load() {
		if simpleRead(handle, &bytes[0], uintptr(len(bytes)), &errorCode) < 0 {
			simpleFree(handle)
			return fmt.Errorf("pa_simple_read failed with PulseAudio error %d: %s", errorCode, strError(errorCode))
		}
		samples := make([]float32, len(bytes)/2)
		for index := range samples {
			samples[index] = float32(int16(binary.NativeEndian.Uint16(bytes[index*2:]))) / math.MaxInt16
		}
		tx.trySend(audioMessage{samples: samples})
	}
	simpleFree(handle)
	return nil
}

// loadPulseFunction binds the Go function *function to the library's symbol
// name, as libloading's Library::get does.
func loadPulseFunction(function any, library uintptr, name string) error {
	symbol, err := purego.Dlsym(library, name)
	if err != nil {
		return fmt.Errorf("load %s: %w", name, err)
	}
	purego.RegisterFunc(function, symbol)
	return nil
}

// cString is text as a NUL-terminated C string. text holds no NUL byte.
func cString(text string) *byte {
	bytes := append([]byte(text), 0)
	return &bytes[0]
}
