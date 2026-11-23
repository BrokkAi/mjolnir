// Port of the cfg(all(test, unix)) tests of
// mj-worker/src/acp/guardian/reviewer.rs and
// mj-worker/src/acp/guardian/reviewer_profile.rs.

//go:build unix

package worker

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func reviewerIsRunning(pid int) bool {
	// Signal 0 only checks whether the recorded child still exists.
	return syscall.Kill(pid, 0) == nil
}

// Hard-won: 747336f25: a reviewer that hung before opening ACP survived cancellation.
func TestDroppingAStartedReviewerBeforeConnectionReapsItsBridge(t *testing.T) {
	temp := t.TempDir()
	script := filepath.Join(temp, "hung-reviewer.py")
	pidFile := filepath.Join(temp, "pid")
	if err := os.WriteFile(script, []byte(`
import os, sys, time
with open(sys.argv[1], "w") as out:
    out.write(str(os.getpid()))
while True:
    time.sleep(1)
`), 0o644); err != nil {
		t.Fatal(err)
	}
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Command = "python3"
	spec.Args = []string{script, pidFile}
	spec.Cwd = temp
	spec.Harness = core.HarnessKindKimi

	ctx, drop := context.WithCancel(context.Background())
	started := make(chan error, 1)
	go func() {
		host, err := reviewerHostStart(ctx, &spec)
		if host != nil {
			host.Close()
		}
		started <- err
	}()
	pid := testSupportWaitFor("the hung reviewer bridge to start", func() int {
		for {
			select {
			case err := <-started:
				panic("reviewer start returned before the bridge connected: " + strconv.FormatBool(err != nil))
			case <-time.After(10 * time.Millisecond):
			}
			if text, err := os.ReadFile(pidFile); err == nil {
				if pid, err := strconv.Atoi(string(text)); err == nil {
					return pid
				}
			}
		}
	})
	drop()

	testSupportWaitFor("the cancelled reviewer bridge to be reaped", func() struct{} {
		for reviewerIsRunning(pid) {
			time.Sleep(10 * time.Millisecond)
		}
		return struct{}{}
	})
}

func TestGuardianProfilesKeepLoginMaterialWithoutSessionHooksAndMCP(t *testing.T) {
	source := t.TempDir()
	for _, harness := range []core.HarnessKind{
		core.HarnessKindCodex,
		core.HarnessKindClaude,
		core.HarnessKindAntigravity,
		core.HarnessKindKimi,
	} {
		home := filepath.Join(source, harness.ID())
		login := filepath.Join(home, harness.CredentialFileName())
		if err := core.AtomicWrite(login, []byte("synthetic login")); err != nil {
			t.Fatal(err)
		}
		if err := core.AtomicWrite(filepath.Join(home, "mcp.json"), []byte(`{"mcpServers":{"mj-agents": {}}}`)); err != nil {
			t.Fatal(err)
		}
		args := harness.HomeCLIArgs(home)
		environment := map[string]string{}
		harness.ConfigureStagedHomeEnvironment(home, environment)
		write := func(path, contents string) {
			t.Helper()
			if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
				t.Fatal(err)
			}
		}
		switch harness {
		case core.HarnessKindCodex:
			write(filepath.Join(home, "config.toml"), "[hooks]\n[mcp_servers.mj-agents]\n")
		case core.HarnessKindClaude:
			write(filepath.Join(home, "settings.json"), `{"hooks":{"PostToolBatch": []},"env":{"CLAUDE_MODEL":"custom"}}`)
			write(filepath.Join(home, ".claude.json"), `{"mcpServers":{"mj-agents": {}}}`)
		case core.HarnessKindAntigravity:
			if err := core.AtomicWrite(filepath.Join(home, "config/plugins/mjolnir-mailbox/hooks.json"), []byte("{}")); err != nil {
				t.Fatal(err)
			}
		}
		profile, err := reviewerProfileStage(harness, environment, args)
		if err != nil {
			t.Fatal(err)
		}
		if profile == "" {
			t.Fatalf("%s staged no profile", harness)
		}
		defer os.RemoveAll(profile)
		reviewer := harness.HomeFromEnvironment(environment[harness.HomeEnv()])
		if !strings.HasPrefix(reviewer, profile) {
			t.Fatalf("reviewer home %s is outside %s", reviewer, profile)
		}
		if copied, err := os.ReadFile(filepath.Join(reviewer, harness.CredentialFileName())); err != nil || string(copied) != "synthetic login" {
			t.Fatalf("login %q %v", copied, err)
		}
		if _, err := os.Stat(filepath.Join(reviewer, "mcp.json")); err == nil {
			t.Fatal("the session's MCP registrations reached the reviewer")
		}
		if _, err := os.Stat(filepath.Join(reviewer, "config/plugins")); err == nil {
			t.Fatal("the session's plugins reached the reviewer")
		}
		switch harness {
		case core.HarnessKindCodex:
			data, err := os.ReadFile(filepath.Join(reviewer, "config.toml"))
			if err != nil {
				t.Fatal(err)
			}
			config := map[string]any{}
			if err := toml.Unmarshal(data, &config); err != nil {
				t.Fatal(err)
			}
			if _, ok := config["hooks"]; ok {
				t.Fatal("hooks kept")
			}
			if _, ok := config["mcp_servers"]; ok {
				t.Fatal("MCP servers kept")
			}
			if config["default_permissions"] != ":read-only" {
				t.Fatalf("permissions %v", config["default_permissions"])
			}
		case core.HarnessKindClaude:
			settings := acpFeaturesJSONFile(t, filepath.Join(reviewer, "settings.json"))
			config := acpFeaturesJSONFile(t, filepath.Join(reviewer, ".claude.json"))
			if _, ok := acpFeaturesGet(settings, "hooks"); ok {
				t.Fatal("hooks kept")
			}
			if model, _ := acpFeaturesPointer(settings, "env", "CLAUDE_MODEL"); model != "custom" {
				t.Fatalf("model %v", model)
			}
			if _, ok := acpFeaturesGet(config, "mcpServers"); ok {
				t.Fatal("MCP servers kept")
			}
		case core.HarnessKindAntigravity:
			if !slices.Equal(args, harness.HomeCLIArgs(reviewer)) {
				t.Fatalf("args %v", args)
			}
		}
		if _, err := os.Stat(filepath.Join(home, "mcp.json")); err != nil {
			t.Fatal("the session's own home changed")
		}
	}
}

func acpFeaturesJSONFile(t *testing.T, path string) any {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return acpFeaturesJSON(t, string(data))
}
