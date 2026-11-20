// Port of the tests in mj-core/src/harness_runtime.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestManagedCacheRootPrefersXdgAndFallsBackWhenItIsEmpty(t *testing.T) {
	// `TempDir` is absolute on every target; a hardcoded Unix-style
	// `/custom-cache` is relative on Windows and trips the absolute-path
	// guard the function promises on all platforms.
	xdg := filepath.Join(os.TempDir(), "mj-xdg-cache")
	home := filepath.Join(os.TempDir(), "mj-home")
	xdgRoot, err := ManagedHarnessCacheRoot(xdg, home)
	if err != nil {
		t.Fatalf("absolute XDG cache root: %v", err)
	}
	if want := filepath.Join(xdg, managedRuntimesDir, managedHarnessesDir); xdgRoot != want {
		t.Fatalf("%q, want %q", xdgRoot, want)
	}

	homeRoot, err := ManagedHarnessCacheRoot("", home)
	if err != nil {
		t.Fatalf("HOME fallback: %v", err)
	}
	if want := filepath.Join(home, ".cache", managedRuntimesDir, managedHarnessesDir); homeRoot != want {
		t.Fatalf("%q, want %q", homeRoot, want)
	}
	nodeRoot, err := ManagedNodeCacheRoot(xdg, home)
	if err != nil {
		t.Fatalf("managed Node cache root: %v", err)
	}
	if want := filepath.Join(xdg, managedRuntimesDir, managedNodeDir); nodeRoot != want {
		t.Fatalf("%q, want %q", nodeRoot, want)
	}

	if _, err := ManagedHarnessCacheRoot("relative-cache", home); err == nil {
		t.Fatal("a relative XDG cache root stays rejected on every target")
	}
}

func TestManagedManifestRejectsADifferentInstallID(t *testing.T) {
	install := ManagedHarnessInstallDir(t.TempDir(), HarnessKindCodex)
	if err := os.MkdirAll(install, 0o755); err != nil {
		t.Fatal(err)
	}
	matches, err := ManagedHarnessManifestMatches(install, HarnessKindCodex, Pin(HarnessKindCodex).InstallID)
	if err != nil || matches {
		t.Fatalf("missing manifest: %v %v", matches, err)
	}

	manifest := ManagedHarnessManifestForHarness(HarnessKindCodex)
	body, err := json.Marshal(manifest)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(install, ManifestFile), body, 0o644); err != nil {
		t.Fatal(err)
	}
	matches, err = ManagedHarnessManifestMatches(install, HarnessKindCodex, Pin(HarnessKindCodex).InstallID)
	if err != nil || !matches {
		t.Fatalf("matching manifest: %v %v", matches, err)
	}
	matches, err = ManagedHarnessManifestMatches(install, HarnessKindCodex, "other-install")
	if err != nil || matches {
		t.Fatalf("different install id: %v %v", matches, err)
	}
}
