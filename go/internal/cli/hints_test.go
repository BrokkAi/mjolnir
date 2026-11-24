// Tests of hints.go, ported from the tests module of mj-cli/src/hints.rs.

package cli

import (
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

func TestAShownHintIsRecordedOnceAndSurvivesAReload(t *testing.T) {
	path := filepath.Join(t.TempDir(), "client-hints.json")
	hints := seenHintsLoadFrom(path)
	if !hints.pending(hintPrefixCollision) {
		t.Fatal("a fresh file has the collision hint pending")
	}
	hints.markShown(hintPrefixCollision)
	if hints.pending(hintPrefixCollision) {
		t.Fatal("the collision hint is still pending after it was shown")
	}
	hints.markShown(hintPrefixKeys)

	reloaded := seenHintsLoadFrom(path)
	if reloaded.pending(hintPrefixCollision) {
		t.Error("the reloaded file has the collision hint pending")
	}
	if reloaded.pending(hintPrefixKeys) {
		t.Error("the reloaded file has the keys hint pending")
	}
}

func queuedHints(path string) pendingHints {
	hints := newPendingHints(seenHintsLoadFrom(path))
	hints.queue(hintPrefixCollision, "collision")
	hints.queue(hintPrefixKeys, "keys")
	return hints
}

// currentNotice is what the bar holds, "" for nothing, with whether it
// holds anything.
func currentNotice(notices chat.Notices) (string, bool) { return notices.Current() }

func assertNotice(t *testing.T, notices chat.Notices, want string) {
	t.Helper()
	if current, ok := currentNotice(notices); !ok || current != want {
		t.Fatalf("the bar holds %q (%v), want %q", current, ok, want)
	}
}

// The launch that restores a conversation reports on that in the notice bar.
// A hint written over it would be spent unread, so it waits — and the file
// must not record it while it waits.
// Hard-won: 2a121ed87a: A live startup marked a hint seen before an attach notice displaced it; this checks the hint stays pending until it is actually drawn, then persists once.
func TestAHintWaitsForTheNoticeBarAndIsRecordedOnlyOnceItIsInIt(t *testing.T) {
	path := filepath.Join(t.TempDir(), "client-hints.json")
	hints := queuedHints(path)
	notices := chat.NewNotices()
	now := time.Now()

	// The surface has not settled: the startup pick or an attach is still
	// running.
	if hints.pump(&notices, false, now) {
		t.Fatal("a hint took the bar before the surface settled")
	}
	if current, ok := currentNotice(notices); ok {
		t.Fatalf("the bar holds %q", current)
	}

	// Settled, but the bar carries the attach's own report.
	notices.Set("Opening session… Esc cancels.")
	if hints.pump(&notices, true, now) {
		t.Fatal("a hint replaced the attach's report")
	}
	assertNotice(t, notices, "Opening session… Esc cancels.")
	if !seenHintsLoadFrom(path).pending(hintPrefixCollision) {
		t.Fatal("the waiting hint was recorded")
	}

	// The attach finished and cleared the bar.
	notices.Clear()
	if !hints.pump(&notices, true, now) {
		t.Fatal("the hint did not take the free bar")
	}
	assertNotice(t, notices, "collision")
	// Recorded only on the pump after the frame that drew it.
	if !seenHintsLoadFrom(path).pending(hintPrefixCollision) {
		t.Fatal("the hint was recorded before a frame drew it")
	}
	if hints.pump(&notices, true, now) {
		t.Fatal("the bar changed during the hint's turn")
	}
	if seenHintsLoadFrom(path).pending(hintPrefixCollision) {
		t.Fatal("the drawn hint was not recorded")
	}
}

// Inside tmux a first launch has two things to say, so the second hint
// follows the first instead of replacing it or being dropped.
// Hard-won: 2a121ed87a: Inside tmux only one of two startup hints was shown; this checks the second waits through the first notice and both become recorded.
func TestTwoQueuedHintsAreBothShownOneAfterTheOther(t *testing.T) {
	path := filepath.Join(t.TempDir(), "client-hints.json")
	hints := queuedHints(path)
	notices := chat.NewNotices()
	now := time.Now()

	if !hints.pump(&notices, true, now) {
		t.Fatal("the first hint did not take the bar")
	}
	assertNotice(t, notices, "collision")
	// Still the collision notice's turn.
	if hints.pump(&notices, true, now) {
		t.Fatal("the bar changed during the collision hint's turn")
	}
	assertNotice(t, notices, "collision")

	later := now.Add(chat.NoticeMinimumDisplay)
	if !hints.pump(&notices, true, later) {
		t.Fatal("the second hint did not follow the first")
	}
	assertNotice(t, notices, "keys")
	if hints.pump(&notices, true, later) {
		t.Fatal("the bar changed during the keys hint's turn")
	}
	assertNotice(t, notices, "keys")

	reloaded := seenHintsLoadFrom(path)
	if reloaded.pending(hintPrefixCollision) {
		t.Error("the collision hint was not recorded")
	}
	if reloaded.pending(hintPrefixKeys) {
		t.Error("the keys hint was not recorded")
	}
}

// A hint already shown on an earlier launch is never queued again.
func TestARecordedHintIsNotQueuedAgain(t *testing.T) {
	path := filepath.Join(t.TempDir(), "client-hints.json")
	seen := seenHintsLoadFrom(path)
	seen.markShown(hintPrefixCollision)
	hints := newPendingHints(seen)
	hints.queue(hintPrefixCollision, "collision")
	hints.queue(hintPrefixKeys, "keys")
	notices := chat.NewNotices()
	if !hints.pump(&notices, true, time.Now()) {
		t.Fatal("the unseen hint did not take the bar")
	}
	assertNotice(t, notices, "keys")
}
