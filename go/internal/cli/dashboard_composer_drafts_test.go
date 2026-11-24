// Port of mj-cli/src/dashboard/composer_drafts.rs tests.

package cli

import (
	"testing"
)

func composerDraftInherited(t *testing.T, cache *composerDraftCache, sessionID string) *string {
	t.Helper()
	draft, ok := cache.get(sessionID)
	if !ok {
		t.Fatalf("no draft for %s", sessionID)
	}
	return draft.InheritedInput
}

func composerDraftText(t *testing.T, cache *composerDraftCache, sessionID string) string {
	t.Helper()
	draft, ok := cache.get(sessionID)
	if !ok {
		t.Fatalf("no draft for %s", sessionID)
	}
	return draft.Text
}

func expectInherited(t *testing.T, got *string, want string) {
	t.Helper()
	if got == nil || *got != want {
		t.Fatalf("inherited input %v, want %q", got, want)
	}
}

func TestAClearedDraftStaysEmptyWhenAStaleSnapshotIsReopened(t *testing.T) {
	var cache composerDraftCache

	if text := cache.open("session-a", "/model g").Text; text != "/model g" {
		t.Fatalf("opened %q", text)
	}
	if text := cache.capture("session-a", "", "/model g").Text; text != "" {
		t.Fatalf("captured %q", text)
	}

	// A later controller reload still reports the old shared field, but this
	// client has already adopted and cleared its composer.
	if text := cache.open("session-a", "/model g").Text; text != "" {
		t.Fatalf("reopened %q", text)
	}
	expectInherited(t, composerDraftInherited(t, &cache, "session-a"), "/model g")
}

func TestSessionsKeepIndependentCurrentTextAndBaselines(t *testing.T) {
	var cache composerDraftCache
	cache.open("session-a", "old a")
	cache.open("session-b", "old b")

	cache.capture("session-a", "new a", "old a")
	cache.capture("session-b", "", "old b")

	if text := composerDraftText(t, &cache, "session-a"); text != "new a" {
		t.Fatalf("session-a %q", text)
	}
	if text := composerDraftText(t, &cache, "session-b"); text != "" {
		t.Fatalf("session-b %q", text)
	}
	expectInherited(t, composerDraftInherited(t, &cache, "session-a"), "old a")
	expectInherited(t, composerDraftInherited(t, &cache, "session-b"), "old b")
}

func TestCapturingAgainKeepsTheFirstSharedBaseline(t *testing.T) {
	var cache composerDraftCache
	cache.open("session-a", "legacy")
	cache.capture("session-a", "edited", "legacy")
	current := cache.capture("session-a", "cleared", "a newer snapshot")

	if current.Text != "cleared" {
		t.Fatalf("text %q", current.Text)
	}
	expectInherited(t, current.InheritedInput, "legacy")
}

func TestTwoTerminalCachesDoNotShareComposerText(t *testing.T) {
	var first, second composerDraftCache
	first.open("session-a", "legacy")
	second.open("session-a", "legacy")

	first.capture("session-a", "first terminal", "legacy")
	second.capture("session-a", "second terminal", "legacy")

	if text := composerDraftText(t, &first, "session-a"); text != "first terminal" {
		t.Fatalf("first %q", text)
	}
	if text := composerDraftText(t, &second, "session-a"); text != "second terminal" {
		t.Fatalf("second %q", text)
	}
}

func TestDelayedAckCoalescesEditsAndClearWithoutBlockingOtherSessions(t *testing.T) {
	var cache composerDraftCache
	cache.capture("a", "typed", "")
	first, _, ok := cache.start("a", false)
	if !ok {
		t.Fatal("start a")
	}
	cache.capture("a", "newer", "")
	cache.capture("a", "", "")
	cache.capture("b", "other", "")
	other, _, ok := cache.start("b", false)
	if !ok {
		t.Fatal("start b")
	}
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("a second save of a started while one is in flight")
	}
	cache.complete("a", first, true)
	if !cache.isSaving() {
		t.Fatal("the other session retains its outstanding ACK")
	}
	latest, draft, ok := cache.start("a", false)
	if !ok {
		t.Fatal("start a again")
	}
	if latest <= first {
		t.Fatalf("latest %d, first %d", latest, first)
	}
	if draft.Text != "" {
		t.Fatalf("sending must durably retire the earlier draft: %q", draft.Text)
	}
	if draft.InheritedInput != nil {
		t.Fatalf("inherited input %q", *draft.InheritedInput)
	}
	cache.complete("a", first, true) // Duplicate old ACK cannot retire latest.
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("a duplicate old ACK retired the latest save")
	}
	cache.complete("a", latest, true)
	cache.complete("b", other, true)
	if cache.isSaving() {
		t.Fatal("still saving")
	}
	cache.capture("a", "", "stale seed")
	if _, _, ok := cache.start("a", true); ok {
		t.Fatal("unchanged navigation/quit cannot write")
	}
}

func TestFailuresRemainDirtyUntilNewEditOrExplicitFlush(t *testing.T) {
	var cache composerDraftCache
	cache.open("empty", "")
	if _, _, ok := cache.start("empty", true); ok {
		t.Fatal("an empty draft started a save")
	}
	cache.open("a", "inherited")
	first, _, ok := cache.start("a", false)
	if !ok {
		t.Fatal("start a")
	}
	cache.complete("a", first, false)
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("a failed save restarted without an edit")
	}
	cache.capture("a", "inherited", "")
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("an unchanged capture restarted a failed save")
	}
	retry, _, ok := cache.start("a", true)
	if !ok {
		t.Fatal("explicit retry")
	}
	if retry != first {
		t.Fatalf("retry %d, first %d", retry, first)
	}
	cache.capture("a", "changed while retrying", "")
	cache.complete("a", retry, false)
	newer, _, ok := cache.start("a", false)
	if !ok {
		t.Fatal("start the newer revision")
	}
	if newer <= retry {
		t.Fatalf("newer %d, retry %d", newer, retry)
	}
	if _, _, ok := cache.start("a", true); ok { // Quit requests final flush.
		t.Fatal("a second save started while one is in flight")
	}
	cache.complete("a", newer, false)
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("quit must not retry the same uncertain in-flight revision")
	}
	if cache.isSaving() {
		t.Fatal("still saving")
	}
	// A later explicit flush may retry a value already known to have failed.
	finalAttempt, _, ok := cache.start("a", true)
	if !ok {
		t.Fatal("final explicit flush")
	}
	if finalAttempt != newer {
		t.Fatalf("final attempt %d, newer %d", finalAttempt, newer)
	}
	cache.complete("a", finalAttempt, false)
	if _, _, ok := cache.start("a", false); ok {
		t.Fatal("failed final flush must not spin")
	}
}
