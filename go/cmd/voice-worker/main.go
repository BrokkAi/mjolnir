// Port of voice-worker/src/main.rs.

// Command voice-worker is mj-voice-worker, the microphone capture sidecar
// for Mjolnir. The parent (mj-chat's speech supervisor) starts it with
// --output PATH, sends one newline on stdin to finish recording and closes
// stdin to cancel. The worker writes the captured audio to PATH as a 16 kHz
// PCM16 WAV file and reports progress as one JSON event per stdout line.
//
// The Rust crate depends on mj-core only for its tests, so this binary does
// not import go/internal/core outside its _test.go files.
package main

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"strings"
	"syscall"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// workerEvent is one stdout line, serde-tagged by "event" with snake_case
// variant names: {"event":"status","message":...}, {"event":"level",
// "value":...}, {"event":"result","text":...}, {"event":"error",
// "message":...}.
//
// Write it with its own MarshalJSON, never through json.Marshal: the
// encoder re-escapes <, > and & in a Marshaler's output, which serde_json
// does not.
type workerEvent interface {
	MarshalJSON() ([]byte, error)
	isWorkerEvent()
}

type workerEventStatus struct{ message string }

type workerEventLevel struct{ value float32 }

type workerEventResult struct{ text string }

type workerEventError struct{ message string }

func (workerEventStatus) isWorkerEvent() {}
func (workerEventLevel) isWorkerEvent()  {}
func (workerEventResult) isWorkerEvent() {}
func (workerEventError) isWorkerEvent()  {}

func (e workerEventStatus) MarshalJSON() ([]byte, error) {
	return appendTaggedString(nil, "status", "message", e.message)
}

func (e workerEventLevel) MarshalJSON() ([]byte, error) {
	b := append([]byte(nil), `{"event":"level","value":`...)
	b = appendSerdeF32(b, e.value)
	return append(b, '}'), nil
}

func (e workerEventResult) MarshalJSON() ([]byte, error) {
	return appendTaggedString(nil, "result", "text", e.text)
}

func (e workerEventError) MarshalJSON() ([]byte, error) {
	return appendTaggedString(nil, "error", "message", e.message)
}

// appendTaggedString appends {"event":<event>,<field>:<value>} with the
// strings escaped as serde_json escapes them.
func appendTaggedString(b []byte, event, field, value string) ([]byte, error) {
	b = append(b, `{"event":`...)
	b, _ = jsonvalue.Append(b, event)
	b = append(b, ',')
	b, _ = jsonvalue.Append(b, field)
	b = append(b, ':')
	b, _ = jsonvalue.Append(b, value)
	return append(b, '}'), nil
}

// appendSerdeF32 appends an f32 as serde_json writes it: null for NaN or an
// infinity, otherwise ryu's shortest f32 digits in ryu's layout ("1.0",
// "0.000012", "1.2e-7", "1e21").
func appendSerdeF32(b []byte, value float32) []byte {
	panic("unported: voice-worker/src/main.rs appendSerdeF32")
}

func emit(event workerEvent) {
	line, err := event.MarshalJSON()
	if err != nil {
		return
	}
	_, _ = os.Stdout.Write(append(line, '\n'))
}

func main() {
	os.Exit(run())
}

func run() int {
	if err := runWithArgs(os.Args[1:]); err != nil {
		fmt.Fprintf(os.Stderr, "voice dictation failed: %v\n", err)
		emit(workerEventError{message: err.Error()})
		return 1
	}
	return 0
}

func runWithArgs(args []string) error {
	outputPath, err := parseOutputPath(args)
	if err != nil {
		return err
	}
	// read_commands sends at most two commands (finish, then cancel or an
	// input error), so this buffer never blocks it, as Rust's unbounded
	// channel never does.
	commands := make(chan stdinCommand, 2)
	go readCommands(commands)

	if _, err := backendRun(
		func(string) {},
		func(value float32) { emit(workerEventLevel{value: value}) },
		func(message string) { emit(workerEventStatus{message: message}) },
		outputPath,
		commands,
	); err != nil {
		return err
	}
	// Both outcomes emit an empty result. Keep the result event in the
	// protocol on cancellation: the parent supervisor treats an empty result
	// as a clean cancelled run.
	emit(workerEventResult{text: ""})
	return nil
}

func parseOutputPath(args []string) (string, error) {
	var outputPath *string
	for i := 0; i < len(args); i++ {
		arg := args[i]
		if arg == "--output" {
			if i+1 >= len(args) {
				return "", errors.New("--output requires a path")
			}
			i++
			value := args[i]
			outputPath = &value
		} else if value, ok := strings.CutPrefix(arg, "--output="); ok && utf8.ValidString(arg) {
			if value == "" {
				return "", errors.New("--output requires a path")
			}
			outputPath = &value
		} else {
			return "", fmt.Errorf("unknown argument %s; usage: mj-voice-worker --output PATH", osStrDebug(arg))
		}
	}
	if outputPath == nil {
		return "", errors.New("missing required --output PATH argument")
	}
	return *outputPath, nil
}

// readCommands reads stdin and sends the worker's commands: a first line
// (or a final line without a newline) finishes recording; EOF cancels,
// before or after that line; a read error is an input error. It closes
// commandTx when it returns, as dropping Rust's sender disconnects the
// receiver.
func readCommands(commandTx chan<- stdinCommand) {
	panic("unported: voice-worker/src/main.rs read_commands")
}

// readLine is BufRead::read_line for a line that is discarded: it returns
// the number of bytes consumed (0 at EOF, which a final line without a
// newline is not), and Rust's "stream did not contain valid UTF-8" error
// for a line that is not UTF-8.
func readLine(reader *bufio.Reader) (int, error) {
	panic("unported: voice-worker/src/main.rs read_line")
}

// osStrDebug renders an argument as Rust's {:?} does for an OsString on
// Unix: a quoted string with char::escape_debug escapes (both quote kinds
// escaped) and each byte of invalid UTF-8 as \xNN in upper-case hex.
func osStrDebug(text string) string {
	panic("unported: voice-worker/src/main.rs osStrDebug")
}

// ioErrorDisplay is the text Rust's io::Error shows: for an OS error the
// system's description and its number (`No such file or directory (os
// error 2)`), otherwise the error itself. It copies core.IOErrorDisplay,
// which this binary does not import (see the package comment).
func ioErrorDisplay(err error) string {
	var errno syscall.Errno
	if !errors.As(err, &errno) {
		return err.Error()
	}
	description := errno.Error()
	if description != "" {
		first, size := utf8.DecodeRuneInString(description)
		description = string(unicode.ToUpper(first)) + description[size:]
	}
	return fmt.Sprintf("%s (os error %d)", description, int(errno))
}
