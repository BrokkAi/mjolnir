// Port of the tests in mj-targets/src/targets/container.rs.

package targets

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func pidsLimitTemplate(extraRunArgs []string) *core.TargetsContainerTemplate {
	return &core.TargetsContainerTemplate{
		Image:        "example.invalid/agent:latest",
		PullPolicy:   core.ImagePullPolicyAuto,
		ExtraRunArgs: extraRunArgs,
	}
}

// A session container holds a parent and its sub-agent children, each with
// a worker, an ACP supervisor and a harness process that all size their
// thread pools to the host. The engine default of 2048 fits about two
// sessions, so Mjolnir asks for room for the concurrency it allows (#1065).
//
// Hard-won: #1065: Concurrent child launches must have more than the engine’s default process slots.
func TestASessionContainerAsksForMoreProcessesThanTheEngineDefault(t *testing.T) {
	for _, engine := range []string{"podman", "docker"} {
		args, err := containerRunArgs(engine, pidsLimitTemplate(nil), "mj-test",
			"0123456789abcdef0123456789abcdef", nil, nil, "/workspace", true)
		if err != nil {
			t.Fatal(err)
		}
		if !slices.Contains(args, "--pids-limit=8192") {
			t.Fatalf("%s run args must raise the process limit: %q", engine, args)
		}
	}
}

// An operator who chose a number keeps it.
func TestAConfiguredProcessLimitIsLeftAlone(t *testing.T) {
	args, err := containerRunArgs("podman", pidsLimitTemplate([]string{"--pids-limit=256"}), "mj-test",
		"0123456789abcdef0123456789abcdef", nil, nil, "/workspace", true)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Contains(args, "--pids-limit=256") {
		t.Fatalf("%q", args)
	}
	count := 0
	for _, argument := range args {
		if strings.HasPrefix(argument, "--pids-limit") {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("%q", args)
	}
}
