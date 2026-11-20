// Port of the tests in mj-checkpoint/src/resources.rs (`cfg(all(test, unix))`).

//go:build unix

package checkpoint

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestResourceDirectoryRoundTripsThroughOneStream(t *testing.T) {
	source := t.TempDir()
	if err := os.MkdirAll(filepath.Join(source, "many/nested"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "many/nested/a.txt"), []byte("alpha"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(source, "empty"), 0o777); err != nil {
		t.Fatal(err)
	}
	destination := filepath.Join(t.TempDir(), "installed")

	err := StreamResource(source, func(stream io.Reader) error {
		return InstallResourceStream(stream, destination)
	})
	if err != nil {
		t.Fatal(err)
	}

	contents, err := os.ReadFile(filepath.Join(destination, "many/nested/a.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(contents, []byte("alpha")) {
		t.Fatalf("%q", contents)
	}
	if info, err := os.Stat(filepath.Join(destination, "empty")); err != nil || !info.IsDir() {
		t.Fatalf("the empty directory was not installed: %v", err)
	}
}

func TestResourceStreamsRejectSymbolicLinks(t *testing.T) {
	source := t.TempDir()
	if err := os.Symlink("outside", filepath.Join(source, "link")); err != nil {
		t.Fatal(err)
	}
	err := StreamResource(source, func(stream io.Reader) error {
		_, err := io.Copy(io.Discard, stream)
		return err
	})
	if err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("%v", err)
	}
}
