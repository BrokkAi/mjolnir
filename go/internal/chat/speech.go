// Port of mj-chat/src/speech.rs.

// Prompt dictation support.
//
// Non-Android platforms record WAV with an `mj-voice-worker` sidecar.
// JSON lines carry progress and completion. A newline ends capture; EOF
// cancels. The chat uploads the recording to the daemon for transcription.

package chat

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// VoiceCommand is a command sent from the chat input to the dictation
// transport.
type VoiceCommand int

const (
	// VoiceCommandFinish ends microphone capture and saves the WAV.
	VoiceCommandFinish VoiceCommand = iota
	// VoiceCommandCancel cancels capture.
	VoiceCommandCancel
)

// The items below are Rust's non-Android `mod worker`.

// voiceWorkerEvent is one JSON line on the worker's stdout. Internally
// tagged by "event" in snake_case.
type voiceWorkerEvent interface {
	json.Marshaler
	isVoiceWorkerEvent()
}

type voiceWorkerEventStatus struct {
	Message string `json:"message"`
}

type voiceWorkerEventPartial struct {
	Text string `json:"text"`
}

type voiceWorkerEventLevel struct {
	Value float32 `json:"value"`
}

type voiceWorkerEventResult struct {
	Text string `json:"text"`
}

type voiceWorkerEventError struct {
	Message string `json:"message"`
}

func (voiceWorkerEventStatus) isVoiceWorkerEvent()  {}
func (voiceWorkerEventPartial) isVoiceWorkerEvent() {}
func (voiceWorkerEventLevel) isVoiceWorkerEvent()   {}
func (voiceWorkerEventResult) isVoiceWorkerEvent()  {}
func (voiceWorkerEventError) isVoiceWorkerEvent()   {}

func (v voiceWorkerEventStatus) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Event   string `json:"event"`
		Message string `json:"message"`
	}{"status", v.Message})
}

func (v voiceWorkerEventPartial) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Event string `json:"event"`
		Text  string `json:"text"`
	}{"partial", v.Text})
}

func (v voiceWorkerEventLevel) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Event string  `json:"event"`
		Value float32 `json:"value"`
	}{"level", v.Value})
}

func (v voiceWorkerEventResult) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Event string `json:"event"`
		Text  string `json:"text"`
	}{"result", v.Text})
}

func (v voiceWorkerEventError) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Event   string `json:"event"`
		Message string `json:"message"`
	}{"error", v.Message})
}

// unmarshalVoiceWorkerEvent decodes the internally tagged form as serde
// does: the "event" tag and the variant's fields are required, other
// members are ignored.
func unmarshalVoiceWorkerEvent(data []byte) (voiceWorkerEvent, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if members == nil {
		return nil, errors.New("invalid type: null, expected internally tagged enum WorkerEvent")
	}
	field := func(name string, destination any) error {
		raw, ok := members[name]
		if !ok || string(raw) == "null" {
			return fmt.Errorf("missing field `%s`", name)
		}
		return json.Unmarshal(raw, destination)
	}
	var tag string
	if err := field("event", &tag); err != nil {
		return nil, err
	}
	switch tag {
	case "status":
		var event voiceWorkerEventStatus
		return event, field("message", &event.Message)
	case "partial":
		var event voiceWorkerEventPartial
		return event, field("text", &event.Text)
	case "level":
		var event voiceWorkerEventLevel
		return event, field("value", &event.Value)
	case "result":
		var event voiceWorkerEventResult
		return event, field("text", &event.Text)
	case "error":
		var event voiceWorkerEventError
		return event, field("message", &event.Message)
	default:
		return nil, fmt.Errorf("unknown variant `%s`, expected one of `status`, `partial`, `level`, `result`, `error`", tag)
	}
}

// parseVoiceWorkerEvent reads one stdout line; nil for anything that is not
// a protocol event.
func parseVoiceWorkerEvent(line string) voiceWorkerEvent {
	event, err := unmarshalVoiceWorkerEvent([]byte(strings.TrimSpace(line)))
	if err != nil {
		return nil
	}
	return event
}

const (
	// voiceCancelGrace is how long after cancellation the worker gets to
	// exit before it is forcefully stopped.
	voiceCancelGrace = 5 * time.Second
	// voiceCaptureTimeout bounds capture and worker startup even if the
	// sidecar never reports a command-ready state. The worker applies the
	// same capture limit.
	voiceCaptureTimeout = 600 * time.Second
	// voiceTranscriptionTimeout is how long the upload/transcription phase
	// may run. The worker's own timeout is independent; this extra slack
	// prevents a hung worker from keeping the chat task alive forever.
	voiceTranscriptionTimeout = 120 * time.Second
	voiceWorkerGrace          = 30 * time.Second
	voicePollInterval         = 50 * time.Millisecond
)

// runVoiceWorker is parent-side dictation: spawn the sidecar and relay its
// events. It forwards cancel's commands to driveVoiceWorker's channel,
// closing that channel when cancel is closed (Rust's dropped sender).
func runVoiceWorker(outputPath string, onPartial func(string), onLevel func(float32), onStatus func(string), cancel *core.Unbounded[VoiceCommand]) (string, error) {
	panic("unported: mj-chat/src/speech.rs run")
}

func voiceWorkerExecutable() (string, error) {
	isFile := func(path string) bool {
		info, err := os.Stat(path)
		return err == nil && info.Mode().IsRegular()
	}
	if path, ok := core.EnvOverrideOS("VOICE_WORKER"); ok {
		if !isFile(path) {
			return "", fmt.Errorf("MJ_VOICE_WORKER does not exist: %s", path)
		}
		return path, nil
	}
	hel, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("locate the mj executable: %w", err)
	}
	name := "mj-voice-worker"
	if runtime.GOOS == "windows" {
		name = "mj-voice-worker.exe"
	}
	worker := filepath.Join(filepath.Dir(hel), name)
	// What to do comes before the path, which can be long enough to push it
	// past the edge of a one-line notice (launch finding R5-6).
	if !isFile(worker) {
		return "", fmt.Errorf("the voice dictation helper is not installed: install mj-voice-worker beside mj or set MJ_VOICE_WORKER (looked for %s)", worker)
	}
	return worker, nil
}

// driveVoiceWorker relays worker events to the UI callbacks and translates
// every way the worker can end — result, reported error, crash, or hang —
// into a result. Non-protocol stdout lines are ignored. The child is started
// with piped stdin, stdout and stderr (stdin may be nil). A closed cancel
// channel is Rust's disconnected receiver and cancels like
// VoiceCommandCancel.
func driveVoiceWorker(child *exec.Cmd, stdin io.WriteCloser, stdout, stderr io.ReadCloser, onPartial func(string), onLevel func(float32), onStatus func(string), cancel <-chan VoiceCommand) (string, error) {
	panic("unported: mj-chat/src/speech.rs drive_worker")
}

// stopVoiceWorker stops the worker and any subprocesses it owns.
func stopVoiceWorker(child *exec.Cmd) {
	panic("unported: mj-chat/src/speech.rs stop_worker")
}

// reapVoiceWorker waits briefly for the worker to exit, killing it if it
// lingers. Nil when the worker could not be reaped.
func reapVoiceWorker(child *exec.Cmd) *os.ProcessState {
	panic("unported: mj-chat/src/speech.rs reap")
}

// readVoiceWorkerTail keeps the last few KB of the worker's stderr for
// crash diagnostics.
func readVoiceWorkerTail(reader io.Reader) string {
	var buffer []byte
	chunk := make([]byte, 1024)
	for {
		read, err := reader.Read(chunk)
		buffer = append(buffer, chunk[:read]...)
		if len(buffer) > 8192 {
			buffer = append(buffer[:0], buffer[len(buffer)-4096:]...)
		}
		if err != nil || read == 0 {
			break
		}
	}
	return strings.ToValidUTF8(string(buffer), "�")
}

// voiceWorkerCrashError explains a worker that vanished without reporting a
// result or an error, without taking the session with it. A nil status
// means the worker could not be reaped.
func voiceWorkerCrashError(status *os.ProcessState, stderrTail string) error {
	message := "voice dictation stopped unexpectedly"
	if status != nil {
		message = "voice dictation " + describeVoiceWorkerExit(status)
	}
	if line, ok := lastMeaningfulLine(stderrTail); ok {
		message += ": " + line
	}
	message += " — the voice worker runs separately, so your session is unaffected"
	return errors.New(message)
}

func describeVoiceWorkerExit(status *os.ProcessState) string {
	if wait, ok := status.Sys().(syscall.WaitStatus); ok && wait.Signaled() {
		signal := int(wait.Signal())
		name := map[int]string{4: " (SIGILL)", 6: " (SIGABRT)", 8: " (SIGFPE)", 11: " (SIGSEGV)"}[signal]
		return fmt.Sprintf("crashed with signal %d%s", signal, name)
	}
	return describeVoiceWorkerExitCode(status)
}

func describeVoiceWorkerExitCode(status *os.ProcessState) string {
	if code := status.ExitCode(); code >= 0 {
		return fmt.Sprintf("exited unexpectedly (code %d)", code)
	}
	return "stopped unexpectedly"
}

func lastMeaningfulLine(stderrTail string) (string, bool) {
	lines := strings.Split(stderrTail, "\n")
	for index := len(lines) - 1; index >= 0; index-- {
		line := strings.TrimSpace(strings.TrimSuffix(lines[index], "\r"))
		if line == "" {
			continue
		}
		if utf8.RuneCountInString(line) > 200 {
			line = string([]rune(line)[:200])
		}
		return line, true
	}
	return "", false
}

// VoiceInputSupported reports whether this build can record dictation.
func VoiceInputSupported() bool {
	_, unavailable := VoiceInputUnavailableReason()
	return !unavailable
}

// VoiceInputUnavailableReason says why this build cannot record dictation,
// as a sentence for the person who pressed the dictation key, or false when
// it can.
func VoiceInputUnavailableReason() (string, bool) {
	if runtime.GOOS == "android" {
		return "Dictation is unavailable on this platform.", true
	}
	if _, err := voiceWorkerExecutable(); err != nil {
		message := err.Error()
		if first, size := utf8.DecodeRuneInString(message); size > 0 {
			message = strings.ToUpper(string(first)) + message[size:]
		}
		return "Dictation is unavailable. " + message + ".", true
	}
	return "", false
}

// RunDictation captures microphone audio to the supplied WAV path.
//
// onPartial receives the cumulative transcript as it grows, onLevel
// receives normalized microphone levels for the input meter, and onStatus
// receives transient progress messages. VoiceCommandFinish stops capture and
// saves the recording; VoiceCommandCancel, or closing cancel (Rust's dropped
// command sender), closes the worker's stdin and returns a cancellation
// error without a transcript.
func RunDictation(outputPath string, onPartial func(string), onLevel func(float32), onStatus func(string), cancel *core.Unbounded[VoiceCommand]) (string, error) {
	if runtime.GOOS == "android" {
		return "", errors.New("voice dictation is not supported on Android")
	}
	return runVoiceWorker(outputPath, onPartial, onLevel, onStatus, cancel)
}

// DictationErrorMessage is the notice for a failed dictation.
func DictationErrorMessage(err error) string {
	message := err.Error()
	if strings.HasPrefix(message, "voice") || strings.HasPrefix(message, "no speech") {
		return message
	}
	if strings.Contains(message, "microphone") {
		return "voice dictation could not use the microphone: " + message
	}
	return "voice dictation failed: " + message
}
