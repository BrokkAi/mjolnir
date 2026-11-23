//go:build unix

// Port of mj-chat/src/speech.rs (fake-worker tests): driveVoiceWorker
// against short shell scripts standing in for the real worker, covering each
// way the child can end.

package chat

import (
	"os/exec"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func driveFakeVoiceWorker(t *testing.T, script string, cancel *core.Unbounded[VoiceCommand]) (string, error, []string, []string) {
	t.Helper()
	command := exec.Command("/bin/sh", "-c", script)
	stdin, err := command.StdinPipe()
	if err != nil {
		t.Fatalf("spawn fake worker: %v", err)
	}
	stdout, err := command.StdoutPipe()
	if err != nil {
		t.Fatalf("spawn fake worker: %v", err)
	}
	stderr, err := command.StderrPipe()
	if err != nil {
		t.Fatalf("spawn fake worker: %v", err)
	}
	if err := command.Start(); err != nil {
		t.Fatalf("spawn fake worker: %v", err)
	}
	var partials, statuses []string
	text, err := driveVoiceWorker(
		command, stdin, stdout, stderr,
		func(text string) { partials = append(partials, text) },
		func(float32) {},
		func(message string) { statuses = append(statuses, message) },
		cancel,
	)
	return text, err, partials, statuses
}

// voiceNeverCancelled is a command channel whose sender stays alive.
func voiceNeverCancelled() *core.Unbounded[VoiceCommand] {
	return core.NewUnbounded[VoiceCommand]()
}

func TestForwardsEventsAndReturnsResult(t *testing.T) {
	script := `
		printf '%s\n' '{"event":"status","message":"listening..."}'
		printf '%s\n' '{"event":"level","value":0.5}'
		printf '%s\n' '{"event":"partial","text":"hello"}'
		printf 'worker log noise\n'
		printf '%s\n' '{"event":"result","text":"hello world"}'
	`
	text, err, partials, statuses := driveFakeVoiceWorker(t, script, voiceNeverCancelled())
	if err != nil {
		t.Fatalf("transcript: %v", err)
	}
	if text != "hello world" {
		t.Fatalf("text = %q", text)
	}
	if !slices.Equal(partials, []string{"hello"}) {
		t.Fatalf("partials = %q", partials)
	}
	if !slices.Equal(statuses, []string{"listening..."}) {
		t.Fatalf("statuses = %q", statuses)
	}
}

func TestErrorEventSurfacesAsError(t *testing.T) {
	script := `
		printf '%s\n' '{"event":"error","message":"microphone capture failed: boom"}'
		exit 1
	`
	_, err, _, _ := driveFakeVoiceWorker(t, script, voiceNeverCancelled())
	if err == nil {
		t.Fatal("error")
	}
	if message := err.Error(); message != "microphone capture failed: boom" {
		t.Fatalf("message = %q", message)
	}
}

func TestWorkerAbortIsContainedAndDescribed(t *testing.T) {
	script := `
		echo 'voice worker crashed during transcription' >&2
		kill -ABRT $$
	`
	_, err, _, _ := driveFakeVoiceWorker(t, script, voiceNeverCancelled())
	if err == nil {
		t.Fatal("crash error")
	}
	message := err.Error()
	for _, want := range []string{
		"signal 6",
		"SIGABRT",
		"voice worker crashed during transcription",
		"session is unaffected",
	} {
		if !strings.Contains(message, want) {
			t.Fatalf("got: %s", message)
		}
	}
}

func TestSilentExitIsReportedWithCode(t *testing.T) {
	_, err, _, _ := driveFakeVoiceWorker(t, "exit 3", voiceNeverCancelled())
	if err == nil {
		t.Fatal("exit error")
	}
	if message := err.Error(); !strings.Contains(message, "exited unexpectedly (code 3)") {
		t.Fatalf("got: %s", message)
	}
}

func TestCancelClosesStdinAndDiscardsWorkerResult(t *testing.T) {
	// EOF is cancellation. Even if a buggy worker emits a result after that
	// signal, it must never reach the canceled chat.
	script := `
		if read -r _; then
			exit 9
		fi
		i=0
		while [ "$i" -lt 70000 ]; do
			printf 'worker noise\n'
			i=$((i + 1))
		done
		printf '%s\n' '{"event":"result","text":"must-not-reach-ui"}'
	`
	cancel := core.NewUnbounded[VoiceCommand]()
	if err := cancel.Send(VoiceCommandCancel); err != nil {
		t.Fatalf("queue cancel: %v", err)
	}
	_, err, _, _ := driveFakeVoiceWorker(t, script, cancel)
	if err == nil {
		t.Fatal("cancel must not return a transcript")
	}
	if !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("got: %v", err)
	}
}

func TestFinishSendsNewlineAndDrainsLargeUploadOutput(t *testing.T) {
	// The worker sees a blank line as Finish and then performs the
	// upload/transcription phase while stdin remains open. The large noise
	// stream proves stdout is drained concurrently with that phase instead
	// of deadlocking at the pipe buffer boundary.
	script := `
		if ! read -r command; then
			exit 9
		fi
		if [ -n "$command" ]; then
			exit 10
		fi
		i=0
		while [ "$i" -lt 70000 ]; do
			printf 'worker noise\n'
			i=$((i + 1))
		done
		printf '%s\n' '{"event":"result","text":"uploaded transcript"}'
	`
	finish := core.NewUnbounded[VoiceCommand]()
	if err := finish.Send(VoiceCommandFinish); err != nil {
		t.Fatalf("queue finish: %v", err)
	}
	text, err, _, _ := driveFakeVoiceWorker(t, script, finish)
	if err != nil {
		t.Fatalf("uploaded transcript: %v", err)
	}
	if text != "uploaded transcript" {
		t.Fatalf("text = %q", text)
	}
}
