//go:build unix

// Port of the tests in mj-accounts/src/grok_usage.rs (Rust:
// `#[cfg(all(test, unix))]`).

package accounts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func fakeGrok(directory, script string) string {
	core.InstallFakeCommand(directory, "grok", script)
	return filepath.Join(directory, "grok")
}

func TestAnAgentThatExitsWithoutAnsweringIsReportedNotAwaited(t *testing.T) {
	directory := t.TempDir()
	fakeGrok(directory, "#!/bin/sh\nexit 0\n")

	_, err := GrokUsageQuery(context.Background(), directory, map[string]string{"PATH": directory})

	if err != (GrokUsageError{Kind: GrokUsageErrorClosed}) {
		t.Fatalf("error = %#v", err)
	}
	if !strings.Contains(err.Error(), "grok agent stdio exited") {
		t.Fatalf("%v", err)
	}
}

// The harness installer and quota process must agree on the install
// receipt and retain its lease, so cache repair cannot replace a running
// CLI.
func TestAManagedQuotaHarnessStaysLeasedUntilItsChildStops(t *testing.T) {
	directory := t.TempDir()
	cache := filepath.Join(directory, "cache")
	root, err := core.ManagedHarnessCacheRoot(cache, "")
	if err != nil {
		t.Fatal(err)
	}
	install := core.ManagedHarnessInstallDir(root, core.HarnessKindGrok)
	bin := filepath.Join(install, "bin")
	if err := os.MkdirAll(bin, 0o755); err != nil {
		t.Fatal(err)
	}
	fakeGrok(bin, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n")
	if err := os.WriteFile(filepath.Join(install, core.LeaseFile), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	manifest, err := json.Marshal(core.ManagedHarnessManifestForHarness(core.HarnessKindGrok))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(install, core.ManifestFile), manifest, 0o644); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	session, err := spawnGrokUsageSession(ctx, directory, map[string]string{
		"PATH":           directory,
		"GROK_HOME":      directory,
		"XDG_CACHE_HOME": cache,
	})
	if err != nil {
		t.Fatal(err)
	}
	lease, err := os.OpenFile(filepath.Join(install, core.LeaseFile), os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Close()
	tryLock := func() error {
		return syscall.Flock(int(lease.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	}
	if err := tryLock(); !errors.Is(err, syscall.EWOULDBLOCK) {
		t.Fatalf("the lease is not held while the child runs: %v", err)
	}
	// Drive the real stdio child; shutdown must retain the lease until it
	// closes stdin and reaps the process, then release it.
	if err := session.writeMessage(map[string]any{"jsonrpc": "2.0", "id": 1, "method": "initialize"}); err != nil {
		t.Fatal(err)
	}
	session.shutdown(ctx)
	if err := tryLock(); err != nil {
		t.Fatalf("lease released after quota child stops: %v", err)
	}
}

// Hard-won: 4de4844fb: a second PATH search changed the CLI under a different child cwd, and inherited GROK_HOME was ignored.
func TestQuotaCLISelectionSurvivesChildCwdAndInheritedHome(t *testing.T) {
	const childRoot = "MJ_GROK_LOOKUP_TEST_ROOT"
	if root, ok := os.LookupEnv(childRoot); ok {
		// This is an isolated child: no other test shares its cwd.
		if err := os.Chdir(filepath.Join(root, "daemon")); err != nil {
			t.Fatal(err)
		}
		marker := filepath.Join(root, "selected")
		env := map[string]string{
			"QUOTA_PROGRAM_MARKER": marker,
			"XDG_CACHE_HOME":       "relative-cache-is-unusable",
			"PATH":                 strings.Join([]string{"relative", filepath.Join(root, "selected-bin")}, string(os.PathListSeparator)),
		}
		if _, err := GrokUsageQuery(context.Background(), filepath.Join(root, "worker"), env); err != (GrokUsageError{Kind: GrokUsageErrorClosed}) {
			t.Fatalf("error = %#v", err)
		}
		if selected, err := os.ReadFile(marker); err != nil || string(selected) != "selected" {
			t.Fatalf("selected = %q, %v", selected, err)
		}
		delete(env, "PATH")
		if _, err := GrokUsageQuery(context.Background(), filepath.Join(root, "worker"), env); err != (GrokUsageError{Kind: GrokUsageErrorClosed}) {
			t.Fatalf("error = %#v", err)
		}
		if selected, err := os.ReadFile(marker); err != nil || string(selected) != "inherited" {
			t.Fatalf("selected = %q, %v", selected, err)
		}
		return
	}
	root := t.TempDir()
	for _, relative := range []string{"daemon", "worker/relative", "selected-bin", "home/bin", "empty"} {
		if err := os.MkdirAll(filepath.Join(root, relative), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	for _, fake := range [][2]string{
		{"selected-bin", "selected"},
		{"worker/relative", "wrong"},
		{"home/bin", "inherited"},
	} {
		fakeGrok(filepath.Join(root, fake[0]),
			fmt.Sprintf("#!/bin/sh\nprintf '%s' > \"$QUOTA_PROGRAM_MARKER\"\nexit 0\n", fake[1]))
	}
	targets.NewIsolatedTest(targets.TestName(
		"mj_accounts::grok_usage::tests",
		"TestQuotaCLISelectionSurvivesChildCwdAndInheritedHome",
	)).
		Env(childRoot, root).
		Env("GROK_HOME", filepath.Join(root, "home")).
		Env("PATH", filepath.Join(root, "empty")).
		IsolatedStore(filepath.Join(root, "child-instance")).
		Run()
}
