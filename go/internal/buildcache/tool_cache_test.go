// Port of the tests in mj-build-cache/src/tool_cache.rs.

package buildcache

import (
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// failSecondDirectory fails creating the Nx volume, the second directory.
type failSecondDirectory struct {
	mu      sync.Mutex
	notices []string
}

func (f *failSecondDirectory) executor() *funcExecutor {
	return &funcExecutor{
		execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
			switch {
			case command.Program == "uname":
				return output(0, "Linux x86_64\n", ""), nil
			case command.Program == "sh" && command.Purpose == "probe mount source filesystem":
				return output(0, "ext4\n", ""), nil
			case command.Program == "mkdir" && len(command.Args) > 0 && strings.HasSuffix(command.Args[len(command.Args)-1], "/.nx"):
				return output(1, "", "fixture Nx volume is unavailable"), nil
			case command.Program == "mkdir":
				return output(0, "", ""), nil
			}
			return core.CommandOutput{}, errors.New("unexpected preparation program " + command.Program)
		},
		notice: func(notice string) {
			f.mu.Lock()
			defer f.mu.Unlock()
			f.notices = append(f.notices, notice)
		},
	}
}

// macHost is a Mac host whose commands all succeed, recording what ran.
type macHost struct {
	mu  sync.Mutex
	ran []string
}

func (m *macHost) executor() *funcExecutor {
	return &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		m.mu.Lock()
		m.ran = append(m.ran, command.Program)
		m.mu.Unlock()
		var stdout string
		switch {
		case command.Program == "uname":
			stdout = "Darwin arm64\n"
		case command.Program == "sh" && command.Purpose == "probe mount source filesystem":
			stdout = "apfs\n"
		case command.Program == "mkdir":
		default:
			return core.CommandOutput{}, errors.New("unexpected preparation program " + command.Program)
		}
		return output(0, stdout, ""), nil
	}}
}

func configWithLocalMachine(settings core.TargetBuildCache) core.Config {
	config := core.DefaultConfig().WithLocalTargets()
	if config.Machines == nil {
		config.Machines = map[string]core.Machine{}
	}
	config.Machines["local"] = core.MachineLocal{TargetBuildCache: &settings}
	return config
}

// Hard-won: #1330: bare sessions on a Mac got no native build caches or mbx launchers.
func TestAMacPreparesNativeCachesForBareSessionsButNotContainers(t *testing.T) {
	requireUnix(t)
	config := configWithLocalMachine(core.TargetBuildCache{ToolsDirectory: stringPointer("/cache/native")})

	bare := &macHost{}
	var mounts []core.AdditionalMount
	placement := ToolCachePrepare(&config, "localhost", &mounts, bare.executor())
	if placement == nil {
		t.Fatal("a bare session on a Mac prepares native caches")
	}
	if placement.Directory != "/cache/native" {
		t.Fatalf("directory = %s", placement.Directory)
	}
	if len(mounts) != 0 {
		t.Fatalf("a bare session mounts nothing: %v", mounts)
	}

	// Its containers run Linux in a VM that these host paths do not describe.
	container := &macHost{}
	if ToolCachePrepare(&config, "podman", &mounts, container.executor()) != nil {
		t.Fatal("a container on a Mac must not get native caches")
	}
	if len(mounts) != 0 {
		t.Fatalf("mounts = %v", mounts)
	}
	if !slices.Equal(container.ran, []string{"uname"}) {
		t.Fatalf("ran = %v", container.ran)
	}
}

func TestFailedCachePreparationCannotPublishAPartialMountSet(t *testing.T) {
	skipOnWindows(t, "#1293: mbx cache host paths and Linux-host checks assume a Unix controller")
	settings := core.TargetBuildCache{ToolsDirectory: stringPointer("/cache/native")}
	config := configWithLocalMachine(settings)
	encodedSettings, err := json.Marshal(settings)
	if err != nil {
		t.Fatal(err)
	}
	template, err := core.UnmarshalTargetTemplateJSON([]byte(
		`{"kind": "local-podman", "image": "fixture", "build_cache": ` + string(encodedSettings) + `}`))
	if err != nil {
		t.Fatal(err)
	}
	config.Targets["fixture"] = template
	original := []core.AdditionalMount{{Source: "/data", Destination: "/data", Access: core.MountAccessRo}}
	mounts := slices.Clone(original)
	executor := &failSecondDirectory{}
	if ToolCachePrepare(&config, "fixture", &mounts, executor.executor()) != nil {
		t.Fatal("a failed preparation publishes no placement")
	}
	if !slices.Equal(mounts, original) {
		t.Fatalf("mounts = %v", mounts)
	}
	if !slices.ContainsFunc(executor.notices, func(notice string) bool {
		return strings.Contains(notice, "fixture Nx volume is unavailable")
	}) {
		t.Fatalf("notices = %v", executor.notices)
	}
}
