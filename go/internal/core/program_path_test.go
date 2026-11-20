// Port of the tests in mj-core/src/program_path.rs.

package core

import (
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
)

// Hard-won: 3bf8fb93: Windows could not find docker.exe or podman.exe because PATH lookup ignored PATHEXT.
func TestPathextExtensionsAreAppendedInOrder(t *testing.T) {
	got := namesWithExtensions("docker", ".COM;.EXE; .cmd ;;")
	want := []string{"docker.COM", "docker.EXE", "docker.cmd"}
	if !slices.Equal(got, want) {
		t.Fatalf("got %q, want %q", got, want)
	}
	defaults := namesWithExtensions("podman", defaultPathext)
	if !slices.Contains(defaults, "podman.EXE") || !slices.Contains(defaults, "podman.CMD") {
		t.Fatalf("defaults %q", defaults)
	}
}

// Hard-won: 3bf8fb93: Windows PATHEXT lookup reported installed Docker and Podman executables missing.
func TestFindsAnExeByItsBareName(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("Windows only")
	}
	directory := t.TempDir()
	exe := filepath.Join(directory, "docker.exe")
	if err := os.WriteFile(exe, nil, 0o644); err != nil {
		t.Fatal(err)
	}
	// NTFS names are case-insensitive; PATHEXT supplies `.EXE`.
	found, ok := FindProgramOnPath("docker", directory, true)
	if !ok || !strings.EqualFold(found, exe) {
		t.Fatalf("%q", found)
	}
	if _, ok := FindProgramOnPath("podman", directory, true); ok {
		t.Fatal("found podman")
	}
}

func TestExecutableSearchSkipsFilesWithoutAnExecuteBit(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	plain := t.TempDir()
	runnable := t.TempDir()
	if err := os.WriteFile(filepath.Join(plain, "tool"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	program := filepath.Join(runnable, "tool")
	if err := os.WriteFile(program, nil, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(program, 0o755); err != nil {
		t.Fatal(err)
	}

	if _, ok := FindProgramOnPath("tool", plain, true); !ok {
		t.Fatal("plain file not found")
	}
	if _, ok := FindExecutableOnPath("tool", plain, true); ok {
		t.Fatal("non-executable file found")
	}

	both := strings.Join([]string{plain, runnable}, string(os.PathListSeparator))
	found, ok := FindExecutableOnPath("tool", both, true)
	if !ok || found != program {
		t.Fatalf("got %q", found)
	}
}
