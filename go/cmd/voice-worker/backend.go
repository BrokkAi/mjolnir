// Port of voice-worker/src/backend.rs.
//
// cpal (Rust's cross-platform audio crate) is replaced by miniaudio through
// github.com/gen2brain/malgo, opened on the host cpal's default_host picks
// (ALSA on Linux, CoreAudio on macOS, WASAPI on Windows). The Linux
// PulseAudio path, which Rust loads with libloading, is in backend_linux.go
// and loads libpulse-simple through github.com/ebitengine/purego.

package main

import (
	"context"
	"fmt"
	"math"
	"os"
	"runtime"
	"strings"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/gen2brain/malgo"
)

// stdinCommand is what stdin tells the worker. The worker accepts one
// newline to finish recording. The parent closes stdin when dictation is
// cancelled; encoding also observes that signal.
type stdinCommand interface{ isStdinCommand() }

type stdinCommandFinish struct{}

type stdinCommandCancel struct{}

type stdinCommandInputError struct{ message string }

func (stdinCommandFinish) isStdinCommand()     {}
func (stdinCommandCancel) isStdinCommand()     {}
func (stdinCommandInputError) isStdinCommand() {}

type runOutcome int

const (
	runOutcomeCaptured runOutcome = iota
	runOutcomeCancelled
)

const dictationTimeout = 600 * time.Second

const noAudioTimeout = 5 * time.Second

const sampleRate uint32 = 16_000

const levelEmitInterval = 80 * time.Millisecond

const maxCaptureBytes = 256 * 1024 * 1024

// audioMessage is Rust's Result<Vec<f32>, String>: samples, or a non-nil
// err carrying the capture error's text.
type audioMessage struct {
	samples []float32
	err     error
}

type audioQueueState struct {
	overflowed   atomic.Bool
	disconnected atomic.Bool
}

// audioSender is the realtime side of the bounded audio queue (Rust's
// mpsc::SyncSender). It never blocks the audio callback: a full queue sets
// overflowed and a receiver that has gone sets disconnected.
type audioSender struct {
	tx chan<- audioMessage
	// receiverGone is closed when the capture loop drops its end of the
	// queue (Rust's TrySendError::Disconnected). A nil channel never is.
	receiverGone <-chan struct{}
	state        *audioQueueState
}

func (s audioSender) trySend(message audioMessage) {
	select {
	case <-s.receiverGone:
		s.state.disconnected.Store(true)
		return
	default:
	}
	select {
	case s.tx <- message:
	default:
		s.state.overflowed.Store(true)
	}
}

type capturedAudio struct {
	samples    []float32
	sampleRate uint32
	commandRx  <-chan stdinCommand
}

type captureOutcome interface{ isCaptureOutcome() }

type captureOutcomeReady struct{ captured capturedAudio }

type captureOutcomeCancelled struct{}

func (captureOutcomeReady) isCaptureOutcome()     {}
func (captureOutcomeCancelled) isCaptureOutcome() {}

// inputSource is an open microphone. close is Rust's Drop: it stops the
// capture and releases the device. The Pulse variant, inputSourcePulse, is
// Linux-only (backend_linux.go).
type inputSource interface {
	isInputSource()
	close()
}

// inputSourceCpal is a miniaudio capture device and the context that owns
// it (Rust's cpal::Stream).
type inputSourceCpal struct {
	context *malgo.AllocatedContext
	stream  *malgo.Device
}

func (inputSourceCpal) isInputSource() {}

func (s inputSourceCpal) close() {
	panic("unported: voice-worker/src/backend.rs InputSource::Cpal drop")
}

// cpalHost is the audio host cpal's default_host picks on this platform, as
// a miniaudio backend, with the name cpal's HostId Debug prints.
type cpalHost struct {
	backend malgo.Backend
	id      string
}

// defaultHost is cpal::default_host: CoreAudio on macOS, WASAPI on Windows
// and ALSA elsewhere (cpal's Linux and BSD host without the jack feature).
func defaultHost() cpalHost {
	switch runtime.GOOS {
	case "darwin":
		return cpalHost{backend: malgo.BackendCoreaudio, id: "CoreAudio"}
	case "windows":
		return cpalHost{backend: malgo.BackendWasapi, id: "Wasapi"}
	default:
		return cpalHost{backend: malgo.BackendAlsa, id: "Alsa"}
	}
}

// defaultInputDevice is cpal's Host::default_input_device: the host's
// default capture device, or nil when it has none.
func defaultInputDevice(context *malgo.AllocatedContext) *malgo.DeviceInfo {
	panic("unported: voice-worker/src/backend.rs default_input_device")
}

// buildInputStream builds an input stream that forwards mono f32 samples at
// the device's native sample rate. Conversion to the upload format happens
// after capture, outside the realtime callback.
func buildInputStream(context *malgo.AllocatedContext, device *malgo.DeviceInfo, tx audioSender) (*malgo.Device, uint32, error) {
	panic("unported: voice-worker/src/backend.rs build_input_stream")
}

func isWSL() bool {
	if version, err := os.ReadFile("/proc/version"); err == nil && utf8.Valid(version) {
		lower := strings.Map(func(r rune) rune {
			if 'A' <= r && r <= 'Z' {
				return r + ('a' - 'A')
			}
			return r
		}, string(version))
		if strings.Contains(lower, "microsoft") || strings.Contains(lower, "wsl") {
			return true
		}
	}
	_, ok := os.LookupEnv("WSL_INTEROP")
	return ok
}

func captureBackendError(message string) {
	fmt.Fprintf(os.Stderr, "voice capture: %s\n", message)
	if isWSL() {
		pulseServer := "unset"
		if _, ok := os.LookupEnv("PULSE_SERVER"); ok {
			pulseServer = "set"
		}
		fmt.Fprintf(os.Stderr, "voice capture: WSL detected; PULSE_SERVER=%s and the WSLg Pulse server must be reachable for microphone input\n", pulseServer)
	}
}

func buildAudioSource(tx audioSender) (inputSource, uint32, error) {
	panic("unported: voice-worker/src/backend.rs build_audio_source")
}

func downmix(samples []float32, channels int) []float32 {
	channels = max(channels, 1)
	mixed := make([]float32, 0, (len(samples)+channels-1)/channels)
	for start := 0; start < len(samples); start += channels {
		frame := samples[start:min(start+channels, len(samples))]
		// Rust's f32 Sum starts from -0.0.
		sum := negativeZero
		for _, sample := range frame {
			sum += sample
		}
		mixed = append(mixed, sum/float32(channels))
	}
	return mixed
}

// negativeZero is -0.0 as an f32 (Go constant arithmetic has no negative
// zero, so -0.0 in source is +0.0).
var negativeZero = float32(math.Copysign(0, -1))

// normalizedLevel normalizes an RMS value into the 0.0..=1.0 meter range
// used by the UI (f32::clamp: NaN stays NaN, -0.0 stays -0.0).
func normalizedLevel(rms float32) float32 {
	level := rms * 18.0
	if level < 0.0 {
		level = 0.0
	}
	if level > 1.0 {
		level = 1.0
	}
	return level
}

func chunkRMS(samples []float32) float32 {
	panic("unported: voice-worker/src/backend.rs chunk_rms")
}

func captureAudio(onLevel func(float32), onStatus func(string), commandRx <-chan stdinCommand) (captureOutcome, error) {
	panic("unported: voice-worker/src/backend.rs capture_audio")
}

// tryRecvCommand is mpsc::Receiver::try_recv: the next command, nil and
// false when none is waiting (Empty), or nil and true once the sender has
// closed the channel and it is drained (Disconnected).
func tryRecvCommand(commandRx <-chan stdinCommand) (stdinCommand, bool) {
	select {
	case command, ok := <-commandRx:
		if !ok {
			return nil, true
		}
		return command, false
	default:
		return nil, false
	}
}

// resampleLinear resamples mono capture data with linear interpolation.
// Dictation uploads are normalized to the 16 kHz format accepted by the
// subscription endpoint.
func resampleLinear(samples []float32, inputRate, outputRate uint32) ([]float32, error) {
	panic("unported: voice-worker/src/backend.rs resample_linear")
}

// encodeWAV encodes mono f32 samples as a PCM16 RIFF/WAVE file.
func encodeWAV(samples []float32, sampleRate uint32) ([]byte, error) {
	panic("unported: voice-worker/src/backend.rs encode_wav")
}

func monitorCancellation(commandRx <-chan stdinCommand, cancel context.CancelFunc) {
	go func() {
		for command := range commandRx {
			switch command.(type) {
			case stdinCommandFinish:
			default: // Cancel or InputError.
				cancel()
				return
			}
		}
		cancel()
	}()
}

func saveCapture(outputPath string, captured capturedAudio) (runOutcome, error) {
	cancellation, cancel := context.WithCancel(context.Background())
	monitorCancellation(captured.commandRx, cancel)
	samples, err := resampleLinear(captured.samples, captured.sampleRate, sampleRate)
	if err != nil {
		return 0, err
	}
	wav, err := encodeWAV(samples, sampleRate)
	if err != nil {
		return 0, err
	}
	if cancellation.Err() != nil {
		return runOutcomeCancelled, nil
	}
	if err := os.WriteFile(outputPath, wav, 0o666); err != nil {
		return 0, fmt.Errorf("write captured WAV: %s", ioErrorDisplay(err))
	}
	return runOutcomeCaptured, nil
}

// backendRun is backend::run (renamed: main.go's run comes first).
func backendRun(_ func(string), onLevel func(float32), onStatus func(string), outputPath string, commandRx <-chan stdinCommand) (runOutcome, error) {
	outcome, err := captureAudio(onLevel, onStatus, commandRx)
	if err != nil {
		return 0, err
	}
	ready, ok := outcome.(captureOutcomeReady)
	if !ok {
		return runOutcomeCancelled, nil
	}
	return saveCapture(outputPath, ready.captured)
}
