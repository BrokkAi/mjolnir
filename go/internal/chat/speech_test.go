// Port of mj-chat/src/speech.rs (tests).

package chat

import (
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 3d0f6d26: the long helper path used to push the install remedy off the notice line.
func TestAMissingVoiceHelperSaysWhatToDoBeforeThePath(t *testing.T) {
	if runtime.GOOS == "android" {
		t.Skip("dictation has no helper on Android")
	}
	// The test binary has no helper beside it; an explicit override is the
	// one setup that would give it one.
	if _, ok := core.EnvOverrideOS("VOICE_WORKER"); ok {
		return
	}
	reason, ok := VoiceInputUnavailableReason()
	if !ok {
		t.Fatal("no helper beside the test binary")
	}
	remedy := strings.Index(reason, "install mj-voice-worker beside mj or set MJ_VOICE_WORKER")
	if remedy < 0 {
		t.Fatal(reason)
	}
	path := strings.Index(reason, "(looked for ")
	if path < 0 {
		t.Fatal(reason)
	}
	if !strings.HasPrefix(reason, "Dictation is unavailable. The voice dictation helper is not installed") {
		t.Fatal(reason)
	}
	if remedy >= path {
		t.Fatal(reason)
	}
}

func TestParseEventIgnoresNonProtocolOutput(t *testing.T) {
	for _, line := range []string{"", "worker startup log line", `{"event":"unknown"}`} {
		if event := parseVoiceWorkerEvent(line); event != nil {
			t.Fatalf("parseVoiceWorkerEvent(%q) = %#v, want nil", line, event)
		}
	}
}

func TestCrashErrorIncludesStderrLineAndIsolatedWorkerHint(t *testing.T) {
	err := voiceWorkerCrashError(nil, "worker startup failed\ntranscription helper exited unexpectedly\n")
	message := err.Error()
	for _, want := range []string{
		"voice dictation stopped unexpectedly",
		"transcription helper exited unexpectedly",
		"session is unaffected",
	} {
		if !strings.Contains(message, want) {
			t.Fatalf("%q does not contain %q", message, want)
		}
	}
}
