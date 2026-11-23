// Port of the tests in mj-client/src/settings/edit.rs.

package client

import (
	"encoding/json"
	"slices"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// settingsJSON reads a test value the way serde_json's json! macro builds
// one.
func settingsJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}

// Reviewers are load-balanced across enabled profiles, so disabling one
// leaves `[review]` as it was.
func TestDisablingAProfileLeavesReviewUnchanged(t *testing.T) {
	review := settingsJSON(t, `{"trigger": "push", "reviewers": [{"model": "gpt-6-terra", "effort": "xhigh"}]}`)
	draft := settingsJSON(t, `{"profiles": {"codex": {"enabled": true}}, "review": {"trigger": "push", "reviewers": [{"model": "gpt-6-terra", "effort": "xhigh"}]}}`)
	notice, err := Toggle(&draft, []string{"profiles", "codex", "enabled"})
	if err != nil {
		t.Fatal(err)
	}
	if notice == nil || *notice != `Disabled profile "codex".` {
		t.Fatalf("notice %v", notice)
	}
	if !settingsEqual(settingsGet(draft, "review"), review) {
		t.Fatalf("review changed: %s", settingsDisplay(settingsGet(draft, "review")))
	}
}

func TestAddingAReviewerToLegacyDraftClearsOldFields(t *testing.T) {
	draft := settingsJSON(t, `{"review": {"trigger": "turn", "count": 3, "profile": "old", "effort": "high"}}`)
	if _, err := EditAddEntry(&draft, []string{"review", "reviewers"}, nil); err != nil {
		t.Fatal(err)
	}
	reviewers, _ := settingsArray(settingsPath(draft, "review", "reviewers"))
	if len(reviewers) != 1 {
		t.Fatalf("reviewers %v", reviewers)
	}
	for _, field := range []string{"count", "profile", "effort"} {
		if settingsHas(settingsGet(draft, "review"), field) {
			t.Errorf("%s survived", field)
		}
	}
}

// The generic list page edits a profile's `models` allow-list as a list of
// strings, and an untouched empty list stays out of the saved file.
func TestAProfileModelsListIsEditedAsAListOfStrings(t *testing.T) {
	config := core.DefaultConfig()
	var profile core.HarnessProfile
	if err := json.Unmarshal([]byte(`{"kind": "open-code", "home": "/home/agent/ds8bit"}`), &profile); err != nil {
		t.Fatal(err)
	}
	config.Profiles["ds8bit"] = profile
	_, draft := DraftOpen(&config)
	if !settingsEqual(settingsPath(draft, "profiles", "ds8bit", "models"), []any{}) {
		t.Fatalf("models %s", settingsDisplay(settingsPath(draft, "profiles", "ds8bit", "models")))
	}
	opened, err := ConfigFromDraft(settingsClone(draft))
	if err != nil {
		t.Fatal(err)
	}
	if len(opened.Profiles["ds8bit"].Models) != 0 {
		t.Fatalf("models %v", opened.Profiles["ds8bit"].Models)
	}

	list := []string{"profiles", "ds8bit", "models"}
	entry, err := EditAddEntry(&draft, list, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ApplyValue(&draft, entry, SettingInputText{Value: "openrouter/deepseek/deepseek-v4.1-flash-1017"}); err != nil {
		t.Fatal(err)
	}
	saved, err := ConfigFromDraft(draft)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(saved.Profiles["ds8bit"].Models, []string{"openrouter/deepseek/deepseek-v4.1-flash-1017"}) {
		t.Fatalf("models %v", saved.Profiles["ds8bit"].Models)
	}
}
