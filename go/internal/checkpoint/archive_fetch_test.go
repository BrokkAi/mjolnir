// Port of the tests in mj-checkpoint/src/archive/fetch.rs.

package checkpoint

import (
	"archive/zip"
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
)

// fetchTestFile is tempfile::tempfile(): an anonymous file, closed when the
// test ends.
func fetchTestFile(t testing.TB) *os.File {
	t.Helper()
	file, err := os.CreateTemp(t.TempDir(), ".tmp")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { file.Close() })
	return file
}

func TestAFetchedPathLandsUnderItsSessionAndCannotLeaveIt(t *testing.T) {
	temp := "/scratch"
	fetched := func(kind FetchedKind, path string) *FetchedPath {
		fetched, err := FetchedPathFromReader(kind, path, bytes.NewReader(nil), fetchTestFile(t))
		if err != nil {
			t.Fatal(err)
		}
		return fetched
	}
	// Rust compares the PathBufs by component, so the result is compared in
	// its /-separated form on every platform.
	location := func(session string, fetched *FetchedPath) string {
		path, err := fetched.LocalPath(temp, session)
		if err != nil {
			t.Fatal(err)
		}
		return filepath.ToSlash(path)
	}
	// Two children's reports share a name; the target path keeps them apart.
	if got := location("d8443f17", fetched(FetchedKindFile, "/workspace/d8443f17/.mj-agents/c1/report.md")); got !=
		"/scratch/mjolnir/d8443f17/workspace/d8443f17/.mj-agents/c1/report.md" {
		t.Fatalf("report lands at %s", got)
	}
	if got := location("d8443f17", fetched(FetchedKindDirectory, "/tmp/rust-veto-cf")); got !=
		"/scratch/mjolnir/d8443f17/tmp/rust-veto-cf.zip" {
		t.Fatalf("directory lands at %s", got)
	}
	// Whatever the target answers, the file stays in the session's directory.
	if got := location("s", fetched(FetchedKindFile, "/a/../../etc/passwd")); got != "/scratch/mjolnir/s/a/etc/passwd" {
		t.Fatalf("traversal lands at %s", got)
	}
	for _, session := range []string{"../s", "a/b", "/s", ".."} {
		if _, err := fetched(FetchedKindFile, "/x").LocalPath(temp, session); err == nil {
			t.Fatalf("%q must not name a directory", session)
		}
	}
}

func fetchRefusal(t testing.TB, fetched *FetchedPath, err error) string {
	t.Helper()
	var refused SessionExportErrorRefused
	if !errors.As(err, &refused) {
		t.Fatalf("expected a refusal, got %v, %v", fetched, err)
	}
	return string(refused)
}

func openFetchedZip(t testing.TB, fetched *FetchedPath) *zip.Reader {
	t.Helper()
	archive, err := zip.NewReader(fetched.Reader(), int64(fetched.Len()))
	if err != nil {
		t.Fatal(err)
	}
	return archive
}

func readFetchedZipEntry(t testing.TB, archive *zip.Reader, name string) []byte {
	t.Helper()
	for _, file := range archive.File {
		if file.Name == name {
			reader, err := file.Open()
			if err != nil {
				t.Fatal(err)
			}
			defer reader.Close()
			contents, err := io.ReadAll(reader)
			if err != nil {
				t.Fatal(err)
			}
			return contents
		}
	}
	t.Fatalf("archive has no entry %s", name)
	return nil
}

func TestAFetchStaysInsideTheSharedRootsAndArchivesLinksWithoutFollowingThem(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("needs Unix symlinks")
	}
	workspace := t.TempDir()
	outside := t.TempDir()
	archiveWriteFile(t, filepath.Join(outside, "secret.txt"), []byte("private\n"))
	evidence := filepath.Join(workspace, "evidence")
	if err := os.MkdirAll(filepath.Join(evidence, "nested", "empty"), 0o777); err != nil {
		t.Fatal(err)
	}
	// More than one pipe buffer, so the archive is not a toy.
	log := make([]byte, 0, 800_000)
	for n := range uint32(200_000) {
		log = binary.LittleEndian.AppendUint32(log, n)
	}
	archiveWriteFile(t, filepath.Join(evidence, "nested", "probe.log"), log)
	archiveWriteFile(t, filepath.Join(evidence, "report.md"), []byte("# Report\n"))
	if err := os.Symlink(filepath.Join(outside, "secret.txt"), filepath.Join(evidence, "leak")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(workspace, "escape")); err != nil {
		t.Fatal(err)
	}
	roots := []string{workspace}

	// A link that resolves outside every root is refused, whether it names
	// the file or a directory above it.
	for _, path := range []string{
		filepath.Join(evidence, "leak"),
		filepath.Join(workspace, "escape", "secret.txt"),
		workspace + "/../" + filepath.Base(outside),
	} {
		fetched, err := FetchSessionPath(path, roots, false, FetchModeContent, fetchTestFile(t))
		if reason := fetchRefusal(t, fetched, err); !strings.Contains(reason, "outside the directories a session shares") {
			t.Fatalf("%s: %s", path, reason)
		}
	}

	fetched, err := FetchSessionPath(evidence, roots, false, FetchModeContent, fetchTestFile(t))
	if err != nil {
		t.Fatal(err)
	}
	if fetched.Kind != FetchedKindDirectory {
		t.Fatalf("kind %s", fetched.Kind)
	}
	wire := fetchTestFile(t)
	if _, err := fetched.WriteTo(wire); err != nil {
		t.Fatal(err)
	}
	if _, err := wire.Seek(0, io.SeekStart); err != nil {
		t.Fatal(err)
	}
	decoded, err := FetchedPathReadFrom(wire)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.Kind != fetched.Kind || decoded.Path != fetched.Path {
		t.Fatalf("decoded %s %s, want %s %s", decoded.Kind, decoded.Path, fetched.Kind, fetched.Path)
	}
	archive := openFetchedZip(t, decoded)
	var names []string
	for _, file := range archive.File {
		names = append(names, file.Name)
	}
	slices.Sort(names)
	if want := []string{
		"evidence/",
		"evidence/leak",
		"evidence/nested/",
		"evidence/nested/empty/",
		"evidence/nested/probe.log",
		"evidence/report.md",
	}; !slices.Equal(names, want) {
		t.Fatalf("entries %q", names)
	}
	if content := readFetchedZipEntry(t, archive, "evidence/nested/probe.log"); !bytes.Equal(content, log) {
		t.Fatal("probe.log differs")
	}
	// The link is stored as a link: its entry holds the target's name, never
	// the secret it points at.
	for _, file := range archive.File {
		if file.Name == "evidence/leak" && file.Mode()&fs.ModeSymlink == 0 {
			t.Fatalf("leak is stored as %v", file.Mode())
		}
	}
	if link := readFetchedZipEntry(t, archive, "evidence/leak"); string(link) != filepath.Join(outside, "secret.txt") {
		t.Fatalf("link target %q", link)
	}
}

// Hard-won: #1543: fetch rejected real artifacts and directories at arbitrary size limits.
func TestFetchTransfersFilesAndArchivesBeyondTheFormerSizeLimits(t *testing.T) {
	workspace := t.TempDir()
	directory := filepath.Join(workspace, "probes")
	if err := os.Mkdir(directory, 0o777); err != nil {
		t.Fatal(err)
	}
	noise := archiveXorshiftBytes(0x2545_f491_4f6c_dd1d, 17*1024*1024)
	file := filepath.Join(directory, "noise.bin")
	archiveWriteFile(t, file, noise)
	roots := []string{workspace}
	fetched, err := FetchSessionPath(file, roots, false, FetchModeContent, fetchTestFile(t))
	if err != nil {
		t.Fatal(err)
	}
	restored, err := io.ReadAll(fetched.Reader())
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(restored, noise) {
		t.Fatal("fetched file differs")
	}
	// This compresses well but exceeds the former directory input limit.
	zeros, err := os.Create(filepath.Join(directory, "zeros.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if err := zeros.Truncate(129 * 1024 * 1024); err != nil {
		t.Fatal(err)
	}
	zeros.Close()
	fetched, err = FetchSessionPath(directory, roots, false, FetchModeContent, fetchTestFile(t))
	if err != nil {
		t.Fatal(err)
	}
	if fetched.Len() <= 16*1024*1024 {
		t.Fatalf("archive of %d bytes", fetched.Len())
	}
	wire := fetchTestFile(t)
	if _, err := fetched.WriteTo(wire); err != nil {
		t.Fatal(err)
	}
	if _, err := wire.Seek(0, io.SeekStart); err != nil {
		t.Fatal(err)
	}
	truncated, err := os.OpenFile(wire.Name(), os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer truncated.Close()
	decoded, err := FetchedPathReadFrom(wire)
	if err != nil {
		t.Fatal(err)
	}
	first, second := decoded.Reader(), decoded.Reader()
	prefix := make([]byte, 128*1024)
	otherPrefix := make([]byte, 128*1024)
	if _, err := io.ReadFull(first, prefix); err != nil {
		t.Fatal(err)
	}
	if _, err := io.ReadFull(second, otherPrefix); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(prefix, otherPrefix) {
		t.Fatal("readers must not share the spool cursor")
	}
	archive := openFetchedZip(t, decoded)
	if content := readFetchedZipEntry(t, archive, "probes/noise.bin"); !bytes.Equal(content, noise) {
		t.Fatal("archived noise differs")
	}
	for _, entry := range archive.File {
		if entry.Name != "probes/zeros.bin" {
			continue
		}
		reader, err := entry.Open()
		if err != nil {
			t.Fatal(err)
		}
		copied, err := io.Copy(io.Discard, reader)
		reader.Close()
		if err != nil {
			t.Fatal(err)
		}
		if copied != 129*1024*1024 {
			t.Fatalf("zeros.bin has %d bytes", copied)
		}
	}
	info, err := truncated.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if err := truncated.Truncate(info.Size() - 1); err != nil {
		t.Fatal(err)
	}
	_, err = FetchedPathReadFrom(truncated)
	archiveAssertErrorContains(t, err, "incomplete fetch transfer")
}
