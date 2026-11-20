// Port of the tests in mj-core/src/path_completion.rs.

package core

import (
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"testing"
)

// macOS filesystems reject invalid UTF-8 names before completion can read them.
func TestNonUTF8EntriesAreSkipped(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("only Linux filesystems accept invalid UTF-8 names")
	}
	directory := t.TempDir()
	if err := os.Mkdir(filepath.Join(directory, "da\xff"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(directory, "data"), 0o755); err != nil {
		t.Fatal(err)
	}
	prefix := directory + "/da"
	if got := LocalCompletions(prefix, CompletionKindAny); !slices.Equal(got, []string{directory + "/data/"}) {
		t.Fatalf("completions = %q", got)
	}
}

func TestLooksLikePathAcceptsSlashTildeDotAndWindowsDrives(t *testing.T) {
	for _, path := range []string{"/srv", "~/cache", "./here", "../there", "C:/users", `d:\x`} {
		if !LooksLikePath(path) {
			t.Errorf("%q is not a path", path)
		}
	}
	for _, other := range []string{
		"owner/repo",
		"https://example.invalid/repo.git",
		"git@example.invalid:owner/repo.git",
		"",
	} {
		if LooksLikePath(other) {
			t.Errorf("%q is a path", other)
		}
	}
}

func TestRemoteLinesAreKeptOnlyWhenTheyExtendThePrefixByOneComponent(t *testing.T) {
	for _, row := range []struct {
		line string
		kind CompletionKind
		kept bool
	}{
		{"/srv/projects/", CompletionKindDirectories, true},
		{"/srv/projects/nested/", CompletionKindDirectories, false},
		{"/srv/prompts.txt", CompletionKindDirectories, false},
		{"/srv/prompts.txt", CompletionKindAny, true},
		{"/srv/pr\uFFFDx/", CompletionKindAny, false},
		{"/srv/pr", CompletionKindAny, false},
		{"/other/pr", CompletionKindAny, false},
	} {
		if got := extendsPrefixByOneComponent(row.line, "/srv/pr", row.kind); got != row.kept {
			t.Errorf("%s %v kept = %v", row.line, row.kind, got)
		}
	}
}
