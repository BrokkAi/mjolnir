// Port of the `cfg(unix)` tests of mj-worker/src/terminal.rs.

//go:build unix

package worker

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestTerminalEnvironmentIsCompleteNotAnAmbientOverlay(t *testing.T) {
	registry, err := NewTerminalRegistry()
	if err != nil {
		t.Fatal(err)
	}
	events := make(chan core.RuntimeEvent, 16)
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	id, err := registry.Create(TerminalSpawn{
		Command:         `printf '%s|%s|%s' "${HOME-unset}" "${CARGO_MANIFEST_DIR-unset}" "$SESSION_SETTING"`,
		Env:             [][2]string{{"SESSION_SETTING", "explicit"}},
		Cwd:             cwd,
		OutputByteLimit: 4096,
	}, events)
	if err != nil {
		t.Fatal(err)
	}
	exit, ok := registry.ExitReceiver(id)
	if !ok {
		t.Fatal("no exit receiver")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	WaitForExit(ctx, exit)
	if ctx.Err() != nil {
		t.Fatal("the terminal did not exit")
	}
	snapshot, ok := registry.Output(id)
	if !ok || snapshot.Output != "unset|unset|explicit" {
		t.Fatalf("output = %q", snapshot.Output)
	}
	registry.Shutdown(context.Background(), events)
}

// A harness restart or worker replacement opens a new ACP connection with a
// new registry, while the session transcript keeps every earlier terminal
// id. Kimi Code's ACP adapter starts every `Bash` call in a client terminal,
// so a reused id attaches the new command's output to a tool call from
// before the restart.
// Hard-won: 69fc70a7: terminal IDs were reused across Kimi connection restarts.
func TestANewConnectionNeverReusesAnEarlierConnectionsTerminalID(t *testing.T) {
	events := make(chan core.RuntimeEvent, 16)
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	spawn := func() TerminalSpawn {
		return TerminalSpawn{Command: "true", Cwd: cwd, OutputByteLimit: 1024}
	}
	beforeRestart, err := NewTerminalRegistry()
	if err != nil {
		t.Fatal(err)
	}
	afterRestart, err := NewTerminalRegistry()
	if err != nil {
		t.Fatal(err)
	}
	create := func(registry *TerminalRegistry) string {
		id, err := registry.Create(spawn(), events)
		if err != nil {
			t.Fatal(err)
		}
		return id
	}
	first := create(beforeRestart)
	second := create(beforeRestart)
	restarted := create(afterRestart)

	if first == second || first == restarted || second == restarted {
		t.Fatalf("ids repeat: %s %s %s", first, second, restarted)
	}
	beforeRestart.Shutdown(context.Background(), events)
	afterRestart.Shutdown(context.Background(), events)
}
