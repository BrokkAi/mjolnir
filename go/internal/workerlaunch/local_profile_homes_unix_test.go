//go:build unix

// Port of the `#[cfg(all(test, unix))] mod tests` in
// mj-worker-launch/src/local_profile_homes.rs.

package workerlaunch

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// noPs is a host without `ps`: every command fails to start.
type noPs struct {
	core.CommandExecutorDefaults
}

func (e noPs) Execute(*core.CommandSpec) (core.CommandOutput, error) {
	return core.CommandOutput{}, errors.New("ps is not installed")
}

func (e noPs) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e noPs) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e noPs) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (e noPs) CancellationRequestedWithoutDeadline() bool { return e.CancellationRequested() }

func (e noPs) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e noPs) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

// A local bare host need not install procps. The startup sweep must still
// see this process instead of leaving checkpoint files in place forever.
// Hard-won: #1516: a host without ps disabled the checkpoint-leftover sweep.
func TestRunningProcessArgumentsFallBackWhenPsIsUnavailable(t *testing.T) {
	own := strings.Join(os.Args, " ")
	running, ok := RunningProcessArguments(noPs{})
	if !ok {
		t.Fatal("list running processes without ps")
	}
	if !slices.Contains(running, own) {
		t.Fatalf("%q not in %d processes", own, len(running))
	}
}

func localProfileHomesSession(id string, harness core.HarnessKind, workerRoot string) core.SessionRecord {
	session := store.CheckpointTestSession(id)
	session.HarnessKind = harness
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: workerRoot}
	return session
}

func localProfileHomesInstallLaunch(t *testing.T, workerRoot string, launch map[string]any) {
	t.Helper()
	if err := os.MkdirAll(workerRoot, 0o777); err != nil {
		t.Fatal(err)
	}
	body, err := json.Marshal(launch)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workerRoot, "launch.json"), body, 0o666); err != nil {
		t.Fatal(err)
	}
}

// A Codex session an earlier release started from `~/.codex` keeps running
// from it: its staged home links there, so a relaunch and a checkpoint find
// its login and its native transcript.
func TestAnEarlierSessionSStagedHomeLinksToTheHomeItsWorkerUses(t *testing.T) {
	directory := t.TempDir()
	profileHome := filepath.Join(directory, ".codex")
	if err := os.MkdirAll(filepath.Join(profileHome, "sessions"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(profileHome, "auth.json"), []byte("{}"), 0o666); err != nil {
		t.Fatal(err)
	}
	earlier := filepath.Join(directory, "workers/earlier")
	localProfileHomesInstallLaunch(t, earlier, map[string]any{
		"harness":      "codex",
		"harness_home": profileHome,
		"environment":  map[string]any{},
	})
	// An older configuration stated the home only through the variable.
	older := filepath.Join(directory, "workers/older")
	localProfileHomesInstallLaunch(t, older, map[string]any{
		"harness":     "codex",
		"environment": map[string]any{"CODEX_HOME": profileHome},
	})
	// A session this release staged already has a home of its own.
	staged := filepath.Join(directory, "workers/staged")
	if err := os.MkdirAll(filepath.Join(staged, "profile"), 0o777); err != nil {
		t.Fatal(err)
	}
	localProfileHomesInstallLaunch(t, staged, map[string]any{
		"harness":      "codex",
		"harness_home": filepath.Join(staged, "profile"),
		"environment":  map[string]any{},
	})
	// A session still provisioning has no installed worker yet.
	provisioning := filepath.Join(directory, "workers/provisioning")
	if err := os.MkdirAll(provisioning, 0o777); err != nil {
		t.Fatal(err)
	}
	state := core.NewStateState()
	for _, entry := range []struct{ id, root string }{
		{"earlier", earlier},
		{"older", older},
		{"staged", staged},
		{"provisioning", provisioning},
	} {
		state.Sessions.Insert(entry.id, localProfileHomesSession(entry.id, core.HarnessKindCodex, entry.root))
	}

	linked := LinkProfileHomesOfEarlierSessions(&state)

	var ids []string
	for _, link := range linked {
		ids = append(ids, link.SessionID)
	}
	if want := []string{"earlier", "older"}; !reflect.DeepEqual(ids, want) {
		t.Fatalf("linked %v, want %v", ids, want)
	}
	for _, id := range []string{"earlier", "older"} {
		workerRoot := state.Sessions.Index(id).Target.(core.TargetLocatorLocalBare).WorkerRoot
		link := filepath.Join(workerRoot, "profile")
		target, err := os.Readlink(link)
		if err != nil {
			t.Fatal(err)
		}
		if target != profileHome {
			t.Fatalf("%s links to %q, want %q", id, target, profileHome)
		}
		if info, err := os.Stat(filepath.Join(link, "auth.json")); err != nil || !info.Mode().IsRegular() {
			t.Fatalf("%s: auth.json through the link is not a file: %v", id, err)
		}
		// The credential sync must not push through the link.
		if SessionHasAStagedHomeOfItsOwn(state.Sessions.Index(id)) {
			t.Fatalf("%s: a linked home counts as the session's own", id)
		}
	}
	if info, err := os.Stat(filepath.Join(staged, "profile")); err != nil || !info.IsDir() {
		t.Fatalf("staged profile is not a directory: %v", err)
	}
	if !SessionHasAStagedHomeOfItsOwn(state.Sessions.Index("staged")) {
		t.Fatal("a staged home does not count as the session's own")
	}
	if _, err := os.Stat(filepath.Join(provisioning, "profile")); err == nil {
		t.Fatal("a provisioning session was given a profile home")
	}

	// A second daemon start finds the links in place and changes nothing.
	if again := LinkProfileHomesOfEarlierSessions(&state); len(again) != 0 {
		t.Fatalf("second start linked %v", again)
	}
}
