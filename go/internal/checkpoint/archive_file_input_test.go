// Port of the tests in mj-checkpoint/src/archive/file_input.rs.

package checkpoint

import (
	"bytes"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

// repeatedByte is std::io::repeat(value): an endless stream of one byte.
type repeatedByte byte

func (r repeatedByte) Read(buffer []byte) (int, error) {
	for index := range buffer {
		buffer[index] = byte(r)
	}
	return len(buffer), nil
}

func TestFileInjectionIsBinaryStreamedAndRequiresExplicitOverwrite(t *testing.T) {
	root := t.TempDir()
	original := make([]byte, 131_337)
	for index := range original {
		original[index] = byte(index % 251)
	}
	path := filepath.Join("nested", "data.bin")
	if err := WriteSessionFile(root, path, original, false); err != nil {
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
	// Files over 16 MiB stream without an artificial cap.
	largeLen := MaxSessionFileBytes + 65_536
	written, err := WriteSessionFileFromReader(root, "large.bin",
		io.LimitReader(repeatedByte(0x5a), int64(largeLen)), &largeLen, false)
	if err != nil {
		t.Fatal(err)
	}
	if written != largeLen {
		t.Fatalf("written = %d", written)
	}
	info, err := os.Stat(filepath.Join(root, "large.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if uint64(info.Size()) != largeLen {
		t.Fatalf("large.bin is %d bytes", info.Size())
	}
}

// failingReader yields remaining bytes of 0xab and then fails as a dropped
// connection does.
type failingReader struct {
	remaining int
}

func (r *failingReader) Read(buffer []byte) (int, error) {
	if r.remaining == 0 {
		return 0, errors.New("simulated mid-stream disconnect")
	}
	count := min(len(buffer), r.remaining)
	for index := range buffer[:count] {
		buffer[index] = 0xab
	}
	r.remaining -= count
	return count, nil
}

func TestStreamingWriteRejectsLengthMismatchAndReadErrorWithoutPublishing(t *testing.T) {
	root := t.TempDir()
	notPublished := func(name string) {
		t.Helper()
		if _, err := os.Stat(filepath.Join(root, name)); !errors.Is(err, fs.ErrNotExist) {
			t.Fatalf("%s exists: %v", name, err)
		}
	}
	// Short read against declared length.
	ten := uint64(10)
	if _, err := WriteSessionFileFromReader(root, "short.bin", bytes.NewReader([]byte("hello")), &ten, false); err == nil {
		t.Fatal("a short upload was published")
	}
	notPublished("short.bin")

	// Trailing extra bytes against declared length.
	five := uint64(5)
	if _, err := WriteSessionFileFromReader(root, "extra.bin", bytes.NewReader([]byte("hello world")), &five, false); err == nil {
		t.Fatal("an overlong upload was published")
	}
	notPublished("extra.bin")

	// Mid-stream read error with unknown length (chunked).
	if _, err := WriteSessionFileFromReader(root, "failed.bin", &failingReader{remaining: 128 * 1024}, nil, false); err == nil {
		t.Fatal("a failed upload was published")
	}
	notPublished("failed.bin")
	// No leftover temp files in root.
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 0 {
		t.Fatalf("root holds %d entries", len(entries))
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
