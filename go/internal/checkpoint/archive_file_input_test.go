// Port of the tests in mj-checkpoint/src/archive/file_input.rs.

package checkpoint

import (
	"bytes"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

// endlessZeros is std::io::repeat(0): an endless stream of zero bytes.
type endlessZeros struct{}

func (endlessZeros) Read(buffer []byte) (int, error) {
	clear(buffer)
	return len(buffer), nil
}

func TestFileInjectionIsBinaryBoundedAndRequiresExplicitOverwrite(t *testing.T) {
	root := t.TempDir()
	original := make([]byte, 131_337)
	for index := range original {
		original[index] = byte(index % 251)
	}
	input, err := ReadSessionFileInput(bytes.NewReader(original))
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join("nested", "data.bin")
	if err := WriteSessionFile(root, path, input, false); err != nil {
		t.Fatal(err)
	}
	if got := archiveReadFile(t, filepath.Join(root, path)); string(got) != string(original) {
		t.Fatal("written file differs")
	}
	if err := WriteSessionFile(root, path, []byte("replacement"), false); err == nil {
		t.Fatal("an existing file was replaced without overwrite")
	}
	if got := archiveReadFile(t, filepath.Join(root, path)); string(got) != string(original) {
		t.Fatal("the refused write changed the file")
	}
	if err := WriteSessionFile(root, path, []byte("replacement"), true); err != nil {
		t.Fatal(err)
	}
	if got := archiveReadFile(t, filepath.Join(root, path)); string(got) != "replacement" {
		t.Fatalf("file = %q", got)
	}
	entries, err := os.ReadDir(filepath.Join(root, "nested"))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 {
		t.Fatalf("nested holds %d entries", len(entries))
	}
	for _, path := range []string{"../outside", "/absolute", "nested/../../outside", ""} {
		if err := WriteSessionFile(root, filepath.FromSlash(path), []byte("bad"), true); err == nil {
			t.Fatalf("%q was written", path)
		}
	}
	if _, err := ReadSessionFileInput(endlessZeros{}); err == nil {
		t.Fatal("an endless input was accepted")
	}
	if err := WriteSessionFile(root, "too-big", make([]byte, MaxSessionFileBytes+1), false); err == nil {
		t.Fatal("an oversized file was written")
	}
	if _, err := os.Stat(filepath.Join(root, "too-big")); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("too-big exists: %v", err)
	}
}

func TestFileInjectionRefusesSymlinksIncludingLinksInsideTheWorkspace(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("needs Unix symlinks")
	}
	root := t.TempDir()
	outside := t.TempDir()
	if err := os.Mkdir(filepath.Join(root, "real"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(root, "escape")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Join(root, "real"), filepath.Join(root, "inside")); err != nil {
		t.Fatal(err)
	}
	archiveWriteFile(t, filepath.Join(root, "target"), []byte("original"))
	if err := os.Symlink(filepath.Join(root, "target"), filepath.Join(root, "leaf")); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{"escape/new/file", "inside/new/file", "leaf"} {
		if err := WriteSessionFile(root, path, []byte("bad"), true); err == nil {
			t.Fatalf("%s was written through a link", path)
		}
	}
	if got := archiveReadFile(t, filepath.Join(root, "target")); string(got) != "original" {
		t.Fatalf("target = %q", got)
	}
	entries, err := os.ReadDir(outside)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 0 {
		t.Fatalf("outside holds %d entries", len(entries))
	}
}
