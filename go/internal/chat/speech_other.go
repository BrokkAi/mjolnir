//go:build !unix

// Port of mj-chat/src/speech.rs (platforms without process groups).

package chat

import (
	"errors"
	"log/slog"
	"os"
	"os/exec"
)

// prepareVoiceWorkerGroup: only Unix gives the worker a process group.
func prepareVoiceWorkerGroup(*exec.Cmd) {}

// stopVoiceWorker stops the worker.
func stopVoiceWorker(child *exec.Cmd) {
	if err := child.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
		slog.Warn("could not stop the voice worker", "error", err)
	}
}
