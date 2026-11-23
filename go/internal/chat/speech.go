// Port of mj-chat/src/speech.rs.

// Prompt dictation support.
//
// Non-Android platforms record WAV with an `mj-voice-worker` sidecar.
// JSON lines carry progress and completion. A newline ends capture; EOF
// cancels. The chat uploads the recording to the daemon for transcription.

package chat

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
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
// events.
func runVoiceWorker(outputPath string, onPartial func(string), onLevel func(float32), onStatus func(string), cancel *core.Unbounded[VoiceCommand]) (string, error) {
	exe, err := voiceWorkerExecutable()
	if err != nil {
		return "", err
	}
	command := exec.Command(exe, "--output", outputPath)
	stdin, err := command.StdinPipe()
	if err != nil {
		return "", fmt.Errorf("start voice worker %s: %w", exe, err)
	}
	stdout, err := command.StdoutPipe()
	if err != nil {
		return "", fmt.Errorf("start voice worker %s: %w", exe, err)
	}
	stderr, err := command.StderrPipe()
	if err != nil {
		return "", fmt.Errorf("start voice worker %s: %w", exe, err)
	}
	prepareVoiceWorkerGroup(command)
	if err := command.Start(); err != nil {
		return "", fmt.Errorf("start voice worker %s: %w", exe, err)
	}
	return driveVoiceWorker(command, stdin, stdout, stderr, onPartial, onLevel, onStatus, cancel)
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

// voiceOutcome is the verdict the drive loop reached; nil (Rust's None)
// when the worker vanished without one.
type voiceOutcome struct {
	text string
	err  error
}

// driveVoiceWorker relays worker events to the UI callbacks and translates
// every way the worker can end — result, reported error, crash, or hang —
// into a result. Non-protocol stdout lines are ignored. The child is started
// with piped stdin, stdout and stderr (stdin and stderr may be nil). A closed
// and drained cancel channel is Rust's disconnected receiver and cancels like
// VoiceCommandCancel.
func driveVoiceWorker(child *exec.Cmd, stdin io.WriteCloser, stdout, stderr io.ReadCloser, onPartial func(string), onLevel func(float32), onStatus func(string), cancel *core.Unbounded[VoiceCommand]) (string, error) {
	if stdout == nil {
		return "", errors.New("voice worker stdout was not captured")
	}

	// A nil event marks stdout EOF: the worker is gone without a verdict.
	events := core.NewUnbounded[voiceWorkerEvent]()
	defer events.Close()
	go func() {
		reader := bufio.NewReader(stdout)
		for {
			line, err := reader.ReadString('\n')
			if line != "" {
				line = strings.TrimSuffix(strings.TrimSuffix(line, "\n"), "\r")
				if !utf8.ValidString(line) {
					slog.Warn("could not read voice worker output", "error", "stream did not contain valid UTF-8")
					break
				}
				if event := parseVoiceWorkerEvent(line); event != nil && events.Send(event) != nil {
					return
				}
			}
			if err != nil {
				// The drive closes stdout once it is done with the worker;
				// that close is not a read failure.
				if !errors.Is(err, io.EOF) && !errors.Is(err, os.ErrClosed) {
					slog.Warn("could not read voice worker output", "error", err)
				}
				break
			}
		}
		// A failed send means the drive already returned and wants no more.
		_ = events.Send(nil)
	}()
	var stderrTail chan string
	if stderr != nil {
		stderrTail = make(chan string, 1)
		go func() { stderrTail <- readVoiceWorkerTail(stderr) }()
	}

	startedAt := time.Now()
	var finishStartedAt, cancelledAt time.Time
	var outcome *voiceOutcome
	for {
		if cancelledAt.IsZero() {
			command, received, closed := cancel.TryRecv()
			switch {
			case received && command == VoiceCommandFinish && finishStartedAt.IsZero():
				if stdin == nil {
					outcome = &voiceOutcome{err: errors.New("voice worker stdin closed before transcription started")}
				} else if _, err := stdin.Write([]byte("\n")); err != nil {
					outcome = &voiceOutcome{err: fmt.Errorf("send voice capture completion to worker: %w", err)}
				} else {
					// Keep stdin open while the worker uploads and
					// transcribes. EOF is reserved for cancellation.
					finishStartedAt = time.Now()
				}
			case received && command == VoiceCommandFinish:
			case received && command == VoiceCommandCancel, closed:
				cancelledAt = time.Now()
				// EOF is the cancellation signal. Do not allow any result
				// the worker may have already queued to reach the canceled
				// chat input.
				if stdin != nil {
					_ = stdin.Close()
					stdin = nil
				}
			}
			if outcome != nil {
				break
			}
		}
		if !cancelledAt.IsZero() && time.Since(cancelledAt) >= voiceCancelGrace {
			stopVoiceWorker(child)
			outcome = &voiceOutcome{err: errors.New("voice dictation cancelled")}
			break
		}
		if !finishStartedAt.IsZero() {
			if time.Since(finishStartedAt) >= voiceTranscriptionTimeout+voiceWorkerGrace {
				stopVoiceWorker(child)
				outcome = &voiceOutcome{err: errors.New("voice dictation transcription timed out")}
				break
			}
		} else if time.Since(startedAt) >= voiceCaptureTimeout+voiceTranscriptionTimeout+voiceWorkerGrace {
			// The worker also finishes capture automatically at its limit,
			// so reserve a complete transcription deadline without a click.
			stopVoiceWorker(child)
			outcome = &voiceOutcome{err: errors.New("voice dictation capture timed out")}
			break
		}
		poll, stopPoll := context.WithTimeout(context.Background(), voicePollInterval)
		event, received := events.Recv(poll)
		timedOut := !received && poll.Err() != nil
		stopPoll()
		if timedOut {
			continue
		}
		if received && event != nil && !cancelledAt.IsZero() {
			// Cancellation owns the result even if the worker raced EOF and
			// emitted one more event.
			continue
		}
		if !received || event == nil {
			if !cancelledAt.IsZero() {
				outcome = &voiceOutcome{err: errors.New("voice dictation cancelled")}
			}
			break
		}
		switch event := event.(type) {
		case voiceWorkerEventPartial:
			onPartial(event.Text)
		case voiceWorkerEventLevel:
			onLevel(event.Value)
		case voiceWorkerEventStatus:
			onStatus(event.Message)
		case voiceWorkerEventResult:
			outcome = &voiceOutcome{text: event.Text}
		case voiceWorkerEventError:
			outcome = &voiceOutcome{err: errors.New(event.Message)}
		}
		if outcome != nil {
			break
		}
	}
	if stdin != nil {
		_ = stdin.Close()
	}

	status := reapVoiceWorker(child)
	tail := ""
	if stderrTail != nil {
		tail = <-stderrTail
		_ = stderr.Close()
	}
	_ = stdout.Close()
	if strings.TrimSpace(tail) != "" {
		if outcome == nil || outcome.err != nil {
			slog.Warn(fmt.Sprintf("voice worker stderr: %s", tail))
		} else {
			slog.Debug(fmt.Sprintf("voice worker stderr: %s", tail))
		}
	}
	if outcome == nil {
		return "", voiceWorkerCrashError(status, tail)
	}
	return outcome.text, outcome.err
}

// reapVoiceWorker waits briefly for the worker to exit, killing it if it
// lingers. Nil when the worker could not be reaped.
//
// It waits on the process rather than the command: Cmd.Wait would close the
// stderr pipe before its tail has been read.
func reapVoiceWorker(child *exec.Cmd) *os.ProcessState {
	type exit struct {
		state *os.ProcessState
		err   error
	}
	exited := make(chan exit, 1)
	go func() {
		state, err := child.Process.Wait()
		exited <- exit{state, err}
	}()
	deadline := time.NewTimer(voiceCancelGrace)
	defer deadline.Stop()
	var result exit
	select {
	case result = <-exited:
		if result.err == nil {
			return result.state
		}
		stopVoiceWorker(child)
	case <-deadline.C:
		stopVoiceWorker(child)
		result = <-exited
	}
	if result.err != nil {
		slog.Warn("could not reap the voice worker", "error", result.err)
		return nil
	}
	return result.state
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
