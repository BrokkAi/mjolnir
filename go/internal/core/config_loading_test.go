package core

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestInstanceNamesAcceptSingleSegmentIdentifiers(t *testing.T) {
	for _, valid := range []string{"dev", "dev-2", "x.y_z", "A1", strings.Repeat("a", 64)} {
		if !IsValidInstanceName(valid) {
			t.Errorf("rejects valid %q", valid)
		}
	}
}

func TestInstanceNamesRejectEmptyAndPathEscapes(t *testing.T) {
	for _, invalid := range []string{"", "   ", ".", "..", "dev/dev", "../evil", `..\evil`,
		"has space", "semi;colon", "uniçode", strings.Repeat("a", 65)} {
		if IsValidInstanceName(invalid) {
			t.Errorf("accepts invalid %q", invalid)
		}
	}
}

func TestApplyInstanceFlagRejectsBadNamesWithoutTouchingTheEnvironment(t *testing.T) {
	// Validation runs before any environment mutation, so these cases cannot
	// leak state even though the environment is process-global.
	for _, invalid := range []string{"", "../evil", "has space"} {
		err := ApplyInstanceFlag(&invalid)
		if err == nil || !strings.Contains(err.Error(), "invalid instance id") {
			t.Errorf("unexpected error for %q: %v", invalid, err)
		}
	}
}

// Hard-won: 4ce2f9a7: test data directory overrides could read or write the user session index
func TestAnOverriddenDataDirectoryGetsItsOwnSessionIndex(t *testing.T) {
	defaultDir := "/home/user/.local/share/mjolnir"
	if _, ok := sessionIndexDirFor(nil, nil, defaultDir); ok {
		t.Fatal("without the override the user's own index is the right one")
	}
	overridden := "/tmp/lab/data"
	if dir, ok := sessionIndexDirFor(nil, &overridden, defaultDir); !ok || dir != filepath.Join("/tmp/lab/data", "sessionwiki") {
		t.Fatalf("a daemon with its own data directory indexes into its own directory, got %q", dir)
	}
	chosen := "/tmp/elsewhere"
	if _, ok := sessionIndexDirFor(&chosen, &overridden, defaultDir); ok {
		t.Fatal("an explicit choice is never overridden")
	}
}

// Hard-won: a daemon started with MJ_DATA_DIR set to the default directory used a stale private index
func TestAnOverrideNamingTheDefaultDataDirectorySharesTheUserIndex(t *testing.T) {
	defaultDir := "/home/user/.local/share/mjolnir"
	for _, same := range []string{"/home/user/.local/share/mjolnir", "/home/user/.local/share/mjolnir/"} {
		if _, ok := sessionIndexDirFor(nil, &same, defaultDir); ok {
			t.Errorf("%s is the default data directory, not an isolated one", same)
		}
	}
}

// Hard-won: 18de1bb4: recovery could act on workers belonging to another instance
func TestInstanceIdentityPrefersAValidInstanceName(t *testing.T) {
	dir := "/home/user/.local/share/mjolnir"
	if got := InstanceIdentityFor("qa0916", dir); got != "qa0916" {
		t.Fatalf("got %q", got)
	}
	if InstanceIdentityFor("../escape", dir) != InstanceIdentityFor("", dir) {
		t.Fatal("an invalid name falls back to the data-dir fingerprint")
	}
}

// Hard-won: 18de1bb4: recovery could act on workers belonging to another instance
func TestInstanceIdentityFingerprintsTheDataDirStably(t *testing.T) {
	first := InstanceIdentityFor("", "/srv/mj/one")
	if first != InstanceIdentityFor("", "/srv/mj/one") || first == InstanceIdentityFor("", "/srv/mj/two") {
		t.Fatal("fingerprint is not stable and distinct")
	}
	// The value the Rust build produces, so both builds agree on worker ownership.
	if first != "beb5f61489a6a79b" {
		t.Fatalf("unexpected fingerprint %q", first)
	}
}

func TestInstanceDirectoriesNestUnderInstancesAndRejectEscapes(t *testing.T) {
	base := filepath.FromSlash("/base/mjolnir")
	if got := withInstanceDir(base, "dev"); got != filepath.Join(base, "instances", "dev") {
		t.Fatalf("got %q", got)
	}
	// An invalid name never becomes a path segment, even if a future caller
	// skips startup validation: it falls back to the base directory.
	for _, instance := range []string{"", "../evil"} {
		if got := withInstanceDir(base, instance); got != base {
			t.Fatalf("%q: got %q", instance, got)
		}
	}
}

// Hard-won: f7272bb0: a development build migrated the live store and replaced its daemon
func TestDevelopmentBuildsMayNotControlTheDefaultStore(t *testing.T) {
	root := t.TempDir()
	defaultStore := filepath.Join(root, "share", "mjolnir")
	namedStore := filepath.Join(defaultStore, "instances", "dev")
	must(t, os.MkdirAll(namedStore, 0o755))
	buildDir := filepath.Join(root, "checkout", "go", "bin")
	must(t, os.MkdirAll(filepath.Join(buildDir, developmentBuildMarker), 0o755))
	must(t, os.MkdirAll(filepath.Join(buildDir, "deps"), 0o755))
	installed := filepath.Join(root, "local", "bin", "mj")
	must(t, os.MkdirAll(filepath.Dir(installed), 0o755))

	for _, executable := range []string{
		filepath.Join(buildDir, "mj"),
		// Linux names a rebuilt executable this way through /proc/self/exe.
		filepath.Join(buildDir, "mj (deleted)"),
		filepath.Join(buildDir, "deps", "mj"),
	} {
		if dir, ok := developmentBuildControllingDefaultStore(executable, defaultStore, defaultStore); !ok || dir != buildDir {
			t.Errorf("%s: got %q %v", executable, dir, ok)
		}
		// The same store reached through a different spelling is still the default.
		if _, ok := developmentBuildControllingDefaultStore(executable, filepath.Join(defaultStore, "instances", ".."), defaultStore); !ok {
			t.Errorf("%s: a respelled default store was not recognised", executable)
		}
		if _, ok := developmentBuildControllingDefaultStore(executable, namedStore, defaultStore); ok {
			t.Errorf("%s: a named instance was refused", executable)
		}
	}
	if _, ok := developmentBuildControllingDefaultStore(installed, defaultStore, defaultStore); ok {
		t.Error("an installed build was refused")
	}
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}
