// Port of voice-worker/src/backend.rs.
//
// cpal (Rust's cross-platform audio crate) is replaced by miniaudio through
// github.com/gen2brain/malgo, opened on the host cpal's default_host picks
// (ALSA on Linux, CoreAudio on macOS, WASAPI on Windows). The Linux
// PulseAudio path, which Rust loads with libloading, is in backend_linux.go
// and loads libpulse-simple through github.com/ebitengine/purego.

package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"math"
	"os"
	"runtime"
	"slices"
	"strings"
	"sync"
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
	s.stream.Uninit()
	closeAudioHost(s.context)
}

// closeAudioHost releases a miniaudio context. ma_context_uninit reports no
// failure a caller could act on (it returns MA_SUCCESS), as dropping cpal's
// host reports none.
func closeAudioHost(context *malgo.AllocatedContext) {
	_ = context.Uninit()
	context.Free()
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
//
// miniaudio marks the default device in its enumeration (on ALSA the PCM
// named "default", the device cpal's ALSA host returns). A host that cannot
// enumerate its capture devices has no default one, as cpal's CoreAudio and
// WASAPI hosts return None when they cannot query it.
func defaultInputDevice(context *malgo.AllocatedContext) *malgo.DeviceInfo {
	devices, err := context.Devices(malgo.Capture)
	if err != nil {
		return nil
	}
	for index := range devices {
		if devices[index].IsDefault != 0 {
			return &devices[index]
		}
	}
	return nil
}

// buildInputStream builds an input stream that forwards mono f32 samples at
// the device's native sample rate. Conversion to the upload format happens
// after capture, outside the realtime callback.
//
// cpal reads the device's default input configuration and then opens the
// stream in it. miniaudio reports the native configuration once the device
// is open in it (an unknown format, no channel count and no sample rate ask
// for the native ones), so the configuration line follows the open.
// Querying the device first keeps cpal's two failure points: a device that
// cannot be queried fails as "query microphone input format", one that
// cannot be opened as "open microphone input stream".
func buildInputStream(context *malgo.AllocatedContext, device *malgo.DeviceInfo, tx audioSender) (*malgo.Device, uint32, error) {
	if _, err := context.DeviceInfo(malgo.Capture, device.ID, malgo.Shared); err != nil {
		return nil, 0, fmt.Errorf("query microphone input format: %w", err)
	}
	config := malgo.DefaultDeviceConfig(malgo.Capture)
	// Pointer copies the ID into C memory, which miniaudio reads while the
	// device opens. The worker opens one device per process, so the copy is
	// not freed.
	config.Capture.DeviceID = device.ID.Pointer()
	config.Capture.Format = malgo.FormatUnknown
	config.Capture.Channels = 0
	config.SampleRate = 0

	// The data callback runs on miniaudio's thread once the stream starts,
	// after the conversion for the negotiated format is stored.
	var convert atomic.Pointer[func([]byte) []float32]
	stream, err := malgo.InitDevice(context.Context, config, malgo.DeviceCallbacks{
		Data: func(_, input []byte, _ uint32) {
			if convert := convert.Load(); convert != nil {
				tx.trySend(audioMessage{samples: (*convert)(input)})
			}
		},
		// miniaudio stops a capture device on its own only when the device
		// fails or goes away (Uninit removes this callback first), which
		// cpal reports to the error callback as
		// StreamError::DeviceNotAvailable.
		Stop: func() {
			tx.trySend(audioMessage{err: errors.New("The requested device is no longer available. For example, it has been unplugged.")})
		},
	})
	if err != nil {
		return nil, 0, fmt.Errorf("open microphone input stream: %w", err)
	}
	sampleFormat := stream.CaptureFormat()
	channels := max(int(stream.CaptureChannels()), 1)
	rate := stream.SampleRate()
	fmt.Fprintf(os.Stderr, "voice capture: device=Ok(%s), rate=%d Hz, channels=%d, format=%s\n",
		osStrDebug(cpalDeviceName(device)), rate, channels, cpalSampleFormatName(sampleFormat))

	var samples func([]byte) []float32
	switch sampleFormat {
	case malgo.FormatF32:
		samples = func(data []byte) []float32 {
			converted := make([]float32, len(data)/4)
			for index := range converted {
				converted[index] = math.Float32frombits(binary.NativeEndian.Uint32(data[index*4:]))
			}
			return downmix(converted, channels)
		}
	case malgo.FormatS16:
		samples = func(data []byte) []float32 {
			converted := make([]float32, len(data)/2)
			for index := range converted {
				converted[index] = float32(int16(binary.NativeEndian.Uint16(data[index*2:]))) / math.MaxInt16
			}
			return downmix(converted, channels)
		}
	// cpal's U16 arm has no counterpart: miniaudio has no unsigned 16-bit
	// format.
	default:
		stream.Uninit()
		return nil, 0, fmt.Errorf("unsupported microphone sample format: %s", cpalSampleFormatName(sampleFormat))
	}
	convert.Store(&samples)
	return stream, rate, nil
}

// cpalDeviceName is the name cpal's Device::name gives the device: the PCM
// name on ALSA ("default"), which is miniaudio's ALSA device ID, and the
// device's display name on CoreAudio and WASAPI, which is miniaudio's name.
func cpalDeviceName(device *malgo.DeviceInfo) string {
	if defaultHost().backend != malgo.BackendAlsa {
		return device.Name()
	}
	id := device.ID[:]
	if end := bytes.IndexByte(id, 0); end >= 0 {
		id = id[:end]
	}
	return string(id)
}

// cpalSampleFormatName is the name cpal's SampleFormat Debug prints for a
// miniaudio sample format.
func cpalSampleFormatName(format malgo.FormatType) string {
	switch format {
	case malgo.FormatU8:
		return "U8"
	case malgo.FormatS16:
		return "I16"
	case malgo.FormatS24:
		return "I24"
	case malgo.FormatS32:
		return "I32"
	case malgo.FormatF32:
		return "F32"
	default:
		return fmt.Sprintf("Unknown(%d)", format)
	}
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
	if source, rate, ok, err := buildPulseSource(tx); ok {
		return source, rate, err
	}

	host := defaultHost()
	fmt.Fprintf(os.Stderr, "voice capture: backend=CPAL, host=%s, WSL=%t\n", host.id, isWSL())
	// cpal links its host into the binary, so opening it cannot fail.
	// miniaudio loads the host's library when its context opens, which
	// fails when that library is missing.
	context, err := malgo.InitContext([]malgo.Backend{host.backend}, malgo.ContextConfig{}, nil)
	if err != nil {
		message := fmt.Sprintf("open the %s audio host: %v", host.id, err)
		captureBackendError(message)
		return nil, 0, errors.New(message)
	}
	device := defaultInputDevice(context)
	if device == nil {
		message := "no microphone input device was found"
		if isWSL() {
			message = "no microphone input device was found through the WSL audio backend; set PULSE_SERVER to the WSLg Pulse server"
		}
		captureBackendError(message)
		closeAudioHost(context)
		return nil, 0, errors.New(message)
	}
	stream, rate, err := buildInputStream(context, device, tx)
	if err != nil {
		captureBackendError(fmt.Sprintf("CPAL microphone stream failed: %v", err))
		closeAudioHost(context)
		return nil, 0, err
	}
	return inputSourceCpal{context: context, stream: stream}, rate, nil
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
	if len(samples) == 0 {
		return 0.0
	}
	// A square is never -0.0, so the sum's starting zero does not matter.
	// The float32 conversion rounds each product on its own, as Rust does,
	// instead of letting the compiler fuse it into the addition.
	var sum float32
	for _, sample := range samples {
		sum += float32(sample * sample)
	}
	return float32(math.Sqrt(float64(sum / float32(len(samples)))))
}

func captureAudio(onLevel func(float32), onStatus func(string), commandRx <-chan stdinCommand) (captureOutcome, error) {
	// An EOF that arrived before the microphone was opened is still a clean
	// cancellation. This keeps process shutdown independent of audio devices.
	var finished bool
	switch command, disconnected := tryRecvCommand(commandRx); command := command.(type) {
	case stdinCommandCancel:
		return captureOutcomeCancelled{}, nil
	case stdinCommandInputError:
		return nil, fmt.Errorf("stdin failed: %s", command.message)
	case stdinCommandFinish:
		finished = true
	case nil:
		if disconnected {
			return captureOutcomeCancelled{}, nil
		}
	}

	audioTx := make(chan audioMessage, 64)
	var audioRx <-chan audioMessage = audioTx
	receiverGone := make(chan struct{})
	// Rust drops the receiver when capture_audio returns, after the source.
	defer close(receiverGone)
	audioState := &audioQueueState{}
	source, micSampleRate, err := buildAudioSource(audioSender{
		tx:           audioTx,
		receiverGone: receiverGone,
		state:        audioState,
	})
	if err != nil {
		return nil, err
	}
	// Rust drops the source on every return, and explicitly where the
	// capture ends; dropSource is both.
	dropSource := sync.OnceFunc(source.close)
	defer dropSource()
	if cpal, ok := source.(inputSourceCpal); ok {
		if err := cpal.stream.Start(); err != nil {
			captureBackendError(fmt.Sprintf("start microphone capture failed: %v", err))
			return nil, fmt.Errorf("start microphone capture: %v", err)
		}
	}
	onStatus("listening...")

	startedAt := time.Now()
	receivedAudio := false
	lastLevelAt := time.Now().Add(-levelEmitInterval)
	var samples []float32
	// audioWait bounds each wait for audio, so commands and the timeouts
	// are checked between chunks (Rust's recv_timeout).
	const audioWait = 30 * time.Millisecond
	wait := time.NewTimer(audioWait)
	defer wait.Stop()

	for !finished {
		if audioState.overflowed.Load() {
			captureBackendError("microphone callback queue overflowed")
			return nil, errors.New("microphone produced audio faster than the worker could process it")
		}
		if audioState.disconnected.Load() {
			return nil, errors.New("microphone capture callback lost its output channel")
		}
		if time.Since(startedAt) >= dictationTimeout {
			break
		}
		// Rust also ends this wait when every sender is gone ("microphone
		// capture stopped unexpectedly"). Here the senders, miniaudio's
		// callbacks and the Pulse goroutine, live until the source closes,
		// and the Pulse goroutine reports its failure before it ends, so
		// the queue cannot lose its senders while this loop runs.
		wait.Reset(audioWait)
		select {
		case message := <-audioRx:
			if message.err != nil {
				captureBackendError(fmt.Sprintf("microphone capture failed: %v", message.err))
				return nil, fmt.Errorf("microphone capture failed: %v", message.err)
			}
			chunk := message.samples
			receivedAudio = true
			if time.Since(lastLevelAt) >= levelEmitInterval {
				onLevel(normalizedLevel(chunkRMS(chunk)))
				lastLevelAt = time.Now()
			}
			if (len(samples)+len(chunk))*4 > maxCaptureBytes {
				captureBackendError("microphone capture exceeded its memory limit")
				return nil, fmt.Errorf("microphone capture exceeded the %d MiB memory limit", maxCaptureBytes/(1024*1024))
			}
			samples = append(samples, chunk...)
		case <-wait.C:
		}

		switch command, disconnected := tryRecvCommand(commandRx); command := command.(type) {
		case stdinCommandFinish:
			finished = true
		case stdinCommandCancel:
			dropSource()
			return captureOutcomeCancelled{}, nil
		case stdinCommandInputError:
			return nil, fmt.Errorf("stdin failed: %s", command.message)
		case nil:
			if disconnected {
				dropSource()
				return captureOutcomeCancelled{}, nil
			}
		}

		if !receivedAudio && time.Since(startedAt) >= noAudioTimeout {
			return nil, fmt.Errorf("microphone delivered no audio within %d seconds; it may be muted, in use by another application, or the audio backend may be incompatible", int64(noAudioTimeout/time.Second))
		}
	}

	// Finish and EOF can arrive back-to-back when a supervisor is shutting
	// down. Drain that second command before turning captured bytes into an
	// upload, so EOF always wins the upload decision.
	switch command, disconnected := tryRecvCommand(commandRx); command := command.(type) {
	case stdinCommandCancel:
		dropSource()
		return captureOutcomeCancelled{}, nil
	case stdinCommandInputError:
		return nil, fmt.Errorf("stdin failed: %s", command.message)
	case nil:
		if disconnected {
			dropSource()
			return captureOutcomeCancelled{}, nil
		}
	}

	dropSource()
	if audioState.overflowed.Load() {
		return nil, errors.New("microphone capture queue overflowed; retry with less system load")
	}
	fmt.Fprintf(os.Stderr, "voice capture: recorded %d samples at %d Hz (%.2f seconds)\n",
		len(samples), micSampleRate, float64(len(samples))/float64(micSampleRate))
	if len(samples) == 0 {
		return nil, errors.New("microphone capture produced no audio")
	}
	return captureOutcomeReady{captured: capturedAudio{
		samples:    samples,
		sampleRate: micSampleRate,
		commandRx:  commandRx,
	}}, nil
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
	if len(samples) == 0 {
		return slices.Clone(samples), nil
	}
	if inputRate == 0 || outputRate == 0 {
		return nil, errors.New("audio sample rates must be non-zero")
	}
	if inputRate == outputRate {
		return slices.Clone(samples), nil
	}
	outputLen := max(uint64(len(samples))*uint64(outputRate)/uint64(inputRate), 1)
	scale := float64(inputRate) / float64(outputRate)
	last := len(samples) - 1
	resampled := make([]float32, outputLen)
	for index := range resampled {
		position := float64(index) * scale
		left := int(math.Floor(position))
		fraction := float32(position - float64(left))
		leftSample := samples[min(left, last)]
		rightSample := samples[min(left+1, last)]
		// The float32 conversion rounds the product on its own, as Rust
		// does, instead of letting the compiler fuse it into the addition.
		resampled[index] = leftSample + float32((rightSample-leftSample)*fraction)
	}
	return resampled, nil
}

// encodeWAV encodes mono f32 samples as a PCM16 RIFF/WAVE file.
func encodeWAV(samples []float32, sampleRate uint32) ([]byte, error) {
	if len(samples) > math.MaxInt/2 {
		return nil, errors.New("audio capture is too large for a WAV file")
	}
	dataLen := len(samples) * 2
	if uint64(dataLen) > math.MaxUint32 {
		return nil, errors.New("audio capture exceeds WAV size limit")
	}
	dataLenU32 := uint32(dataLen)
	if dataLenU32 > math.MaxUint32-36 {
		return nil, errors.New("audio capture exceeds RIFF size limit")
	}
	riffLen := 36 + dataLenU32
	if sampleRate > math.MaxUint32/2 {
		return nil, errors.New("sample rate is too large for a WAV file")
	}
	byteRate := sampleRate * 2

	wav := make([]byte, 0, 44+dataLen)
	wav = append(wav, "RIFF"...)
	wav = binary.LittleEndian.AppendUint32(wav, riffLen)
	wav = append(wav, "WAVE"...)
	wav = append(wav, "fmt "...)
	wav = binary.LittleEndian.AppendUint32(wav, 16)
	wav = binary.LittleEndian.AppendUint16(wav, 1)
	wav = binary.LittleEndian.AppendUint16(wav, 1)
	wav = binary.LittleEndian.AppendUint32(wav, sampleRate)
	wav = binary.LittleEndian.AppendUint32(wav, byteRate)
	wav = binary.LittleEndian.AppendUint16(wav, 2)
	wav = binary.LittleEndian.AppendUint16(wav, 16)
	wav = append(wav, "data"...)
	wav = binary.LittleEndian.AppendUint32(wav, dataLenU32)
	for _, sample := range samples {
		wav = binary.LittleEndian.AppendUint16(wav, uint16(pcm16(sample)))
	}
	return wav, nil
}

// pcm16 is `(sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16`:
// f32::round rounds half away from zero, as math.Round does, and a NaN,
// which clamp keeps, converts to 0.
func pcm16(sample float32) int16 {
	if math.IsNaN(float64(sample)) {
		return 0
	}
	scaled := min(max(sample, -1.0), 1.0) * math.MaxInt16
	return int16(math.Round(float64(scaled)))
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
