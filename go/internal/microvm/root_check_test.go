//go:build linux

// Port of the tests of mj-microvm/src/root_check.rs.

package microvm

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func microvmImageTree(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	for _, directory := range []string{"usr/bin", "usr/local/bin", "var/lib", "bin.real"} {
		microvmMustMkdirAll(t, filepath.Join(root, directory))
	}
	microvmMustSymlink(t, "usr/bin", filepath.Join(root, "bin"))
	busybox := filepath.Join(root, "usr/bin/busybox")
	microvmMustWrite(t, busybox, []byte("\x7fELF one-file-system unlink-first strip-components no-recursion occurrence"))
	microvmMustChmod(t, busybox, 0o755)
	for _, program := range []string{"sh", "tar", "flock"} {
		// Absolute links resolve against the image, not the host.
		microvmMustSymlink(t, "/bin/busybox", filepath.Join(root, "usr/bin", program))
	}
	return root
}

const microvmTestPath = "/usr/local/bin:/usr/bin:/bin"

func TestBusyboxStyleImageWithSymlinkedDirectoriesPasses(t *testing.T) {
	root := microvmImageTree(t)
	if problems := microvmRootCheckProblems(root, microvmTestPath); len(problems) != 0 {
		t.Fatalf("%q", problems)
	}
}

// Security boundary: a hostile image's absolute link or `..` must not
// reach the host, and a link loop must end.
func TestLookupsNeverLeaveTheImageAndLinkLoopsEnd(t *testing.T) {
	root := microvmImageTree(t)
	microvmMustWrite(t, filepath.Join(filepath.Dir(root), "host-secret"), []byte("x"))
	microvmMustSymlink(t, "/../../../../etc/passwd", filepath.Join(root, "usr/bin/escape"))
	// The host's /etc/passwd exists, the image's does not.
	if resolved, ok, err := microvmRootCheckResolve(root, "/usr/bin/escape"); err != nil || ok {
		t.Fatalf("%q, %v, %v", resolved, ok, err)
	}
	microvmMustSymlink(t, "../../../host-secret", filepath.Join(root, "usr/bin/dots"))
	if resolved, ok, err := microvmRootCheckResolve(root, "/usr/bin/dots"); err != nil || ok {
		t.Fatalf("%q, %v, %v", resolved, ok, err)
	}
	microvmMustSymlink(t, "loop-b", filepath.Join(root, "usr/bin/loop-a"))
	microvmMustSymlink(t, "loop-a", filepath.Join(root, "usr/bin/loop-b"))
	if _, _, err := microvmRootCheckResolve(root, "/usr/bin/loop-a"); err == nil {
		t.Fatal("a link loop resolved")
	}
}

// Hard-won: review of the first version found a BusyBox-style tar passing a
// check the restore's GNU-only options would then fail after the swap.
func TestATarWithoutTheRestoreOptionsIsNamed(t *testing.T) {
	root := microvmImageTree(t)
	tar := filepath.Join(root, "usr/bin/tar")
	if err := os.Remove(tar); err != nil {
		t.Fatal(err)
	}
	microvmMustWrite(t, tar, []byte("\x7fELF no-recursion strip-components"))
	microvmMustChmod(t, tar, 0o755)
	named := microvmRootCheckProblems(root, microvmTestPath)
	if len(named) != 1 {
		t.Fatalf("%q", named)
	}
	if !strings.Contains(named[0], "unlink-first, occurrence") || strings.Contains(named[0], "strip-components") {
		t.Fatalf("%q", named)
	}
}

func TestEachMissingRequirementIsNamed(t *testing.T) {
	root := microvmImageTree(t)
	if err := os.Remove(filepath.Join(root, "usr/bin/flock")); err != nil {
		t.Fatal(err)
	}
	named := microvmRootCheckProblems(root, microvmTestPath)
	if len(named) != 1 || !strings.Contains(named[0], "`flock`") {
		t.Fatalf("%q", named)
	}

	// A file that is not executable does not count, and a PATH that misses
	// the directory does not find it.
	microvmMustChmod(t, filepath.Join(root, "usr/bin/busybox"), 0o644)
	if named := microvmRootCheckProblems(root, microvmTestPath); len(named) != 3 {
		t.Fatalf("%q", named)
	}
	microvmMustChmod(t, filepath.Join(root, "usr/bin/busybox"), 0o755)
	if named := microvmRootCheckProblems(root, "/opt/none"); len(named) != 3 {
		t.Fatalf("%q", named)
	}

	// A link to nothing occupies the name, so `mkdir -p` fails on it, at the
	// last component and at an ancestor.
	microvmMustSymlink(t, "/nowhere", filepath.Join(root, "var/lib/hel"))
	if err := microvmCreatableDirectory(root, microvmControlRoot); err == nil {
		t.Fatal("a dangling link at the last component passed")
	}
	if err := os.Remove(filepath.Join(root, "var/lib/hel")); err != nil {
		t.Fatal(err)
	}
	microvmMustSymlink(t, "/nowhere", filepath.Join(root, "var/lib/dangling"))
	if err := microvmCreatableDirectory(root, "/var/lib/dangling/hel"); err == nil {
		t.Fatal("a dangling link at an ancestor passed")
	}
	if err := microvmCreatableDirectory(root, microvmControlRoot); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(root, "var/lib/dangling")); err != nil {
		t.Fatal(err)
	}

	// `mkdir -p /var/lib/hel` cannot pass through a file.
	if err := os.Remove(filepath.Join(root, "var/lib")); err != nil {
		t.Fatal(err)
	}
	microvmMustWrite(t, filepath.Join(root, "var/lib"), []byte("file"))
	named = microvmRootCheckProblems(root, microvmTestPath)
	found := false
	for _, problem := range named {
		found = found || strings.Contains(problem, "/var/lib/hel cannot be created")
	}
	if !found {
		t.Fatalf("%q", named)
	}
}
