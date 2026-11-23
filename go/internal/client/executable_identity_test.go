//go:build linux || darwin

// Port of the tests in mj-client/src/executable.rs.

package client

import (
	"os"
	"path/filepath"
	"testing"
)

func TestExecutableIdentityDetectsAnNfsStyleReplacedBinary(t *testing.T) {
	directory := t.TempDir()
	current := filepath.Join(directory, "mj")
	hardLink := filepath.Join(directory, "mj-hard-link")
	retained := filepath.Join(directory, ".nfs0000000000000001")

	if err := os.WriteFile(current, []byte("old executable"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(current, hardLink); err != nil {
		t.Fatal(err)
	}
	identity := func(path string) ExecutableFileIdentity {
		t.Helper()
		found, err := ExecutableFileIdentityOf(path)
		if err != nil {
			t.Fatal(err)
		}
		return found
	}
	if identity(current) != identity(hardLink) {
		t.Fatal("two names for the same executable inode must compare equal")
	}

	if err := os.Rename(current, retained); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(current, []byte("new executable"), 0o644); err != nil {
		t.Fatal(err)
	}
	if identity(retained) == identity(current) {
		t.Fatal("an NFS-retained old executable must differ from its replacement")
	}
}
