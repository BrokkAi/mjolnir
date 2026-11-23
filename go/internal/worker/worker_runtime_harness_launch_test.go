// Port of the tests in mj-worker/src/worker_runtime/harness_launch.rs.

//go:build unix

package worker

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 7da1ad2bc: a pre-native dsh-acp incorrectly skipped managed Node provisioning.
func TestDshUsesTheManagedNodeRuntimeWithoutANativeLoader(t *testing.T) {
	ctx := context.Background()
	needed, err := needsNodeRuntime(ctx, core.HarnessKindDsh, map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	if !needed {
		t.Fatal("a missing PATH must select the managed Node fallback, not fail discovery")
	}

	tools := t.TempDir()
	current := fmt.Sprintf("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dsh-acp %s'; fi\n", core.DshACPVersion)
	core.InstallFakeCommand(tools, "dsh-acp", current)
	environment := map[string]string{"PATH": tools}
	needed, err = needsNodeRuntime(ctx, core.HarnessKindDsh, environment)
	if err != nil {
		t.Fatal(err)
	}
	if needed {
		t.Fatal("a native dsh-acp needs no Node.js")
	}

	core.InstallFakeCommand(tools, "dsh-acp", "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dsh-acp 0.3.0'; fi\n")
	needed, err = needsNodeRuntime(ctx, core.HarnessKindDsh, environment)
	if err != nil {
		t.Fatal(err)
	}
	if !needed {
		t.Fatal("a pre-native loader needs the pinned Node.js npx fallback")
	}
}

func TestReviewerAndPrimaryHomesReadTheSameReplacedMachinePolicy(t *testing.T) {
	root := t.TempDir()
	shared := filepath.Join(root, "shared")
	if err := os.Mkdir(shared, 0o777); err != nil {
		t.Fatal(err)
	}
	policy := filepath.Join(shared, "config.toml")
	if err := core.AtomicWrite(policy, []byte("[target]\nmax_size = '100GiB'\n")); err != nil {
		t.Fatal(err)
	}
	for _, home := range []string{"primary", "reviewer"} {
		environment := map[string]string{
			"MJ_MBX_CONFIG_DIR": shared,
			"XDG_CONFIG_HOME":   filepath.Join(root, home),
		}
		if err := linkBuildCacheConfiguration(context.Background(), environment); err != nil {
			t.Fatal(err)
		}
		if environment["XDG_CONFIG_HOME"] != filepath.Join(root, home) {
			t.Fatalf("XDG_CONFIG_HOME = %s", environment["XDG_CONFIG_HOME"])
		}
	}
	if err := core.AtomicWrite(policy, []byte("[target]\nmax_size = '300GiB'\n")); err != nil {
		t.Fatal(err)
	}
	for _, home := range []string{"primary", "reviewer"} {
		body, err := os.ReadFile(filepath.Join(root, home, "mbx/config.toml"))
		if err != nil {
			t.Fatal(err)
		}
		if string(body) != "[target]\nmax_size = '300GiB'\n" {
			t.Fatalf("%s reads %q", home, body)
		}
	}
}

// TestAMissingMachinePolicyDoesNotRefuseTheHarness: a failed optional cache
// must not refuse the harness. The controller deliberately starts a session
// without the shared cache when it cannot verify the in-container mbx copy,
// but the worker then refused to start because the unpublished policy was
// absent.
func TestAMissingMachinePolicyDoesNotRefuseTheHarness(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "home")
	missing := filepath.Join(root, "missing")
	if err := os.MkdirAll(filepath.Join(home, "mbx"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Join(missing, "config.toml"), filepath.Join(home, "mbx/config.toml")); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{
		"MJ_MBX_CONFIG_DIR": missing,
		"XDG_CONFIG_HOME":   home,
	}
	if err := linkBuildCacheConfiguration(context.Background(), environment); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(home, "mbx/config.toml")); err == nil {
		t.Fatal("no machine policy may be visible")
	}
}
