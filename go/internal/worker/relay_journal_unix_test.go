//go:build unix

// Port of the #[cfg(unix)] test of mj-worker/src/relay/journal/tests.rs.

package worker

import (
	"os"
	"path/filepath"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// The journal holds the full session transcript, so every file and directory
// it creates must be owner-only (#1409).
func TestRelayJournalFilesAndDirectoriesAreOwnerOnly(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "transcript content")

	journal, active := journalTestPath(temp)
	mode := func(path string) os.FileMode {
		t.Helper()
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		return info.Mode().Perm()
	}
	if got := mode(journal); got != 0o700 {
		t.Fatalf("journal directory mode %o", got)
	}
	if got := mode(active); got != 0o600 {
		t.Fatalf("active segment mode %o", got)
	}
	if got := mode(filepath.Join(temp, core.RelayStateFile)); got != 0o600 {
		t.Fatalf("relay state mode %o", got)
	}

	index := slices.IndexFunc(relay.journalSpans, func(span RelayJournalSpan) bool {
		return filepath.Base(span.path) == core.RelayActiveSegment
	})
	metadata := relay.journalSpans[index]
	if err := sealActiveRelaySegment(journal, &metadata); err != nil {
		t.Fatal(err)
	}
	if got := mode(metadata.path); got != 0o600 {
		t.Fatalf("sealed segment mode %o", got)
	}
	if got := mode(active); got != 0o600 {
		t.Fatalf("replacement active segment mode %o", got)
	}
}
