// Port of the tests in mj-cli/src/dashboard/upgrade.rs.

package cli

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestVersionOneHandoffsRemainReadableByFutureBuilds(t *testing.T) {
	// A fixed previous-build document, independent of today's serializer.
	// New fields must be optional so a skipped release can still resume.
	var resume upgradeResume
	if err := json.Unmarshal([]byte(`{
            "version": 1, "workspace_id": "work", "client_id": "terminal-1",
            "drafts": {"drafts": {"session-1": {"text": "unfinished prompt", "inherited_input": "old"}}},
            "questions": {}, "positions": {"session-1": "Bottom"},
            "layouts": {}, "read_positions": {"session-1": 42}
        }`), &resume); err != nil {
		t.Fatal(err)
	}
	draft, ok := resume.Drafts.get("session-1")
	if !ok || draft.Text != "unfinished prompt" {
		t.Fatalf("draft = %#v, %v", draft, ok)
	}
	if resume.ReadPositions["session-1"] != 42 {
		t.Fatalf("read positions = %v", resume.ReadPositions)
	}
	if resume.WorkspaceID != "work" {
		t.Fatalf("workspace = %q", resume.WorkspaceID)
	}
	revision, started, ok := resume.Drafts.start("session-1", false)
	if !ok {
		t.Fatal("the resumed draft must start a save")
	}
	if revision != 1 {
		t.Fatalf("revision = %d", revision)
	}
	if started.Text != "unfinished prompt" {
		t.Fatalf("started draft = %#v", started)
	}
}

func TestHandoffRetriesUncertainRevisionWithoutReplayingAcknowledgedText(t *testing.T) {
	var drafts composerDraftCache
	drafts.capture("session", "first", "legacy")
	saved, _, ok := drafts.start("session", false)
	if !ok {
		t.Fatal("first save did not start")
	}
	drafts.complete("session", saved, true)
	drafts.capture("session", "in flight", "stale legacy")
	uncertain, _, ok := drafts.start("session", false)
	if !ok {
		t.Fatal("second save did not start")
	}
	drafts.capture("session", "", "stale legacy")
	encoded, err := json.Marshal(drafts)
	if err != nil {
		t.Fatal(err)
	}
	var resumed composerDraftCache
	if err := json.Unmarshal(encoded, &resumed); err != nil {
		t.Fatal(err)
	}
	if resumed.isSaving() {
		t.Fatal("a resumed cache has no save in flight")
	}
	latest, draft, ok := resumed.start("session", false)
	if !ok {
		t.Fatal("the resumed cache must retry the uncertain revision")
	}
	if latest <= uncertain {
		t.Fatalf("latest revision %d must be above the uncertain %d", latest, uncertain)
	}
	if draft.Text != "" {
		t.Fatalf("draft text = %q", draft.Text)
	}
	if draft.InheritedInput != nil {
		t.Fatalf("inherited input = %q", *draft.InheritedInput)
	}
	resumed.complete("session", latest, true)
	encoded, err = json.Marshal(resumed)
	if err != nil {
		t.Fatal(err)
	}
	var clean composerDraftCache
	if err := json.Unmarshal(encoded, &clean); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := clean.start("session", true); ok {
		t.Fatal("an acknowledged draft must not be saved again")
	}
}

func TestAFailedExecRetainsLargeDraftsAndClearedInputs(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_UPGRADE_HANDOFF") {
		return
	}
	var drafts composerDraftCache
	text := strings.Repeat("unsent 🦀\n", 16_000)
	drafts.capture("edited", text, "shared baseline")
	drafts.capture("cleared", "", "do not resurrect")
	resume := upgradeResume{
		Version:       1,
		WorkspaceID:   "workspace-original",
		ClientID:      "terminal-original",
		Drafts:        drafts,
		Questions:     map[string][]cachedQuestionDraft{},
		Positions:     map[string]chat.TranscriptPosition{},
		Layouts:       map[string]core.ConversationLayout{},
		ReadPositions: map[string]uint64{"edited": 123},
	}
	err := resume.restart(UpgradeTarget{
		Executable: filepath.Join(core.DataDir(), "missing-executable"),
		Generation: "test-daemon",
	})
	if err == nil || !strings.Contains(err.Error(), "restart upgraded terminal") {
		t.Fatalf("error = %v", err)
	}
	path := filepath.Join(core.DataDir(), "terminal-upgrades", fmt.Sprintf("%d.json", os.Getpid()))
	saved, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(saved) <= 64*1024 {
		t.Fatalf("saved handoff is only %d bytes", len(saved))
	}
	var reloaded upgradeResume
	if err := json.Unmarshal(saved, &reloaded); err != nil {
		t.Fatal(err)
	}
	if reloaded.WorkspaceID != "workspace-original" {
		t.Fatalf("workspace = %q", reloaded.WorkspaceID)
	}
	if reloaded.ClientID != "terminal-original" {
		t.Fatalf("client = %q", reloaded.ClientID)
	}
	if edited, ok := reloaded.Drafts.get("edited"); !ok || edited.Text != text {
		t.Fatal("the large draft was not retained")
	}
	cleared, ok := reloaded.Drafts.get("cleared")
	if !ok || cleared.Text != "" {
		t.Fatalf("cleared draft = %#v, %v", cleared, ok)
	}
	if cleared.InheritedInput == nil || *cleared.InheritedInput != "do not resurrect" {
		t.Fatalf("cleared inherited input = %v", cleared.InheritedInput)
	}
	if reloaded.ReadPositions["edited"] != 123 {
		t.Fatalf("read positions = %v", reloaded.ReadPositions)
	}
	if runtime.GOOS != "windows" {
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		if mode := info.Mode().Perm(); mode != 0o600 {
			t.Fatalf("handoff mode = %o", mode)
		}
	}
}
