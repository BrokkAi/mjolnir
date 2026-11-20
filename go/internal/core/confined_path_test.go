// Port of the tests in mj-core/src/confined_path.rs.

package core

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

// The only case plants a symbolic link, which needs Unix.
// Hard-won: #1419: a directory swapped for a link between the check and the write redirected the write.
func TestADirectorySwappedForALinkAfterOpeningDoesNotRedirectChanges(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("planting the link needs Unix")
	}
	temporary := t.TempDir()
	rootPath := filepath.Join(temporary, "root")
	outside := filepath.Join(temporary, "outside")
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(os.MkdirAll(filepath.Join(rootPath, "notes"), 0o755))
	must(os.MkdirAll(outside, 0o755))
	must(os.WriteFile(filepath.Join(rootPath, "notes/old.md"), []byte("old"), 0o644))
	must(os.WriteFile(filepath.Join(outside, "old.md"), []byte("victim"), 0o644))

	root, err := ConfinedDirOpenRoot(rootPath)
	must(err)
	notes, err := root.OpenDir("notes")
	must(err)
	must(os.Rename(filepath.Join(rootPath, "notes"), filepath.Join(rootPath, "moved")))
	must(os.Symlink(outside, filepath.Join(rootPath, "notes")))

	if err := notes.WriteAtomically("new.md", []byte("sync content"), PublishCreateNew); err != nil {
		t.Fatal(err)
	}
	must(notes.RemoveFile("old.md"))

	if content, err := os.ReadFile(filepath.Join(rootPath, "moved/new.md")); err != nil || string(content) != "sync content" {
		t.Fatalf("moved/new.md = %q, %v", content, err)
	}
	if _, err := os.Stat(filepath.Join(rootPath, "moved/old.md")); err == nil {
		t.Fatal("moved/old.md survived")
	}
	if content, err := os.ReadFile(filepath.Join(outside, "old.md")); err != nil || string(content) != "victim" {
		t.Fatalf("outside/old.md = %q, %v", content, err)
	}
	if _, err := os.Stat(filepath.Join(outside, "new.md")); err == nil {
		t.Fatal("the write followed the planted link")
	}
	// A fresh walk refuses the planted link rather than following it.
	if _, err := root.OpenDir("notes"); err == nil {
		t.Fatal("a fresh walk followed the planted link")
	}
}
