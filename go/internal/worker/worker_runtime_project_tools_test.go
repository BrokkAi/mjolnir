// Port of the tests in mj-worker/src/worker_runtime/project_tools.rs.

//go:build unix

package worker

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// Hard-won: review of 562a30751: an image whose tools were current skipped `mise install`, and with it a changed postinstall hook.
func TestPostinstallHookRunsUnlessTheImageWasBuiltFromThisRecipe(t *testing.T) {
	checkout := t.TempDir()
	baked := t.TempDir()
	recipe := "[tools]\nnode = \"24\"\n[hooks]\npostinstall = \"npx playwright@1.62.1 install\"\n"
	write := func(path, body string) {
		t.Helper()
		if err := os.WriteFile(path, []byte(body), 0o666); err != nil {
			t.Fatal(err)
		}
	}
	pending := func(baked string) *string {
		t.Helper()
		hook, err := pendingPostinstallHook(context.Background(), checkout, baked)
		if err != nil {
			t.Fatal(err)
		}
		return hook
	}
	isHook := func(hook *string, expected string) bool {
		return hook != nil && *hook == expected
	}
	write(filepath.Join(checkout, "mise.toml"), recipe)
	write(filepath.Join(checkout, ".nvmrc"), "24\n")
	write(filepath.Join(baked, "mise.toml"), recipe)
	// Same hook, but the image lacks a recipe file the checkout has.
	if hook := pending(baked); !isHook(hook, "mise.toml") {
		t.Fatalf("hook = %v", hook)
	}
	write(filepath.Join(baked, ".nvmrc"), "24\n")
	if hook := pending(baked); hook != nil {
		t.Fatalf("hook = %v", *hook)
	}
	write(filepath.Join(checkout, "mise.toml"), strings.ReplaceAll(recipe, "1.62.1", "1.63.0"))
	if hook := pending(baked); !isHook(hook, "mise.toml") {
		t.Fatalf("hook = %v", hook)
	}
	if hook := pending(filepath.Join(baked, "absent")); !isHook(hook, "mise.toml") {
		t.Fatalf("hook = %v", hook)
	}
}

// Hard-won: review of a045f704a: a version mismatch was not counted, so a recipe pinning a package the image has at another version skipped `apply`.
func TestPackageVersionMismatchesNeedApplyExceptForAnyVersion(t *testing.T) {
	// Output of mise 2026.11.9 on buildpack-deps:noble.
	status, err := jsonvalue.Parse([]byte(`{"apt": {"available": true, "packages": [
            {"package": "curl", "requested_version": "latest", "desired_state": "present",
             "state": "installed", "installed_version": "8.5.0-2ubuntu10.16"},
            {"package": "git", "requested_version": "1:2.30.0-1", "desired_state": "present",
             "state": "version_mismatch", "installed_version": "1:2.43.0-1ubuntu7.4"},
            {"package": "cowsay", "requested_version": "latest", "desired_state": "present",
             "state": "missing", "installed_version": ""},
            {"package": "make", "requested_version": "*", "desired_state": "present",
             "state": "version_mismatch", "installed_version": "4.3-4.1build2"}
        ]}}`))
	if err != nil {
		t.Fatal(err)
	}
	if got := unsatisfiedPackages(status); !slices.Equal(got, []string{"apt:git=1:2.30.0-1", "apt:cowsay"}) {
		t.Fatalf("unsatisfied = %q", got)
	}
}

// Hard-won: review of b0f32d6: restoring the mise shims put them ahead of the build-cache launchers, bypassing mbx for mise-installed cmake.
func TestBuildCacheLaunchersStayAheadOfProjectShims(t *testing.T) {
	environment := map[string]string{
		"PATH":              "/managed-node/bin:/wrap:/usr/bin",
		"MJ_BUILD_TOOL_BIN": "/wrap",
		"MISE_DATA_DIR":     "/data",
	}
	if err := projectToolsApplyEnvironment(environment, []string{"/workspace/repo"}, nil); err != nil {
		t.Fatal(err)
	}
	if got := environment["PATH"]; got != "/wrap:/data/shims:/managed-node/bin:/usr/bin" {
		t.Fatalf("PATH = %s", got)
	}
}
