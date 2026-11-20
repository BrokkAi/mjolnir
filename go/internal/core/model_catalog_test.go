// Port of the tests in mj-core/src/model_catalog.rs.

package core

import (
	"encoding/json"
	"os"
	"slices"
	"sort"
	"testing"
)

// loadRecordedModels reads tests/fixtures/claude-antigravity-models.json, a
// recording of the models Claude and Antigravity advertise.
func loadRecordedModels(t *testing.T, path string) map[string][]SessionConfigChoice {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var recorded map[string][]SessionConfigChoice
	if err := json.Unmarshal(data, &recorded); err != nil {
		t.Fatal(err)
	}
	return recorded
}

// Hard-won: #1534: independently interpreting a moving alias can mix model versions.
func TestMovingAliasResolvesOnceAndPreservesVersionsContextAndUnknownIDs(t *testing.T) {
	recorded := loadRecordedModels(t, "testdata/model_catalog/claude-antigravity-models.json")
	keys := make([]string, 0, len(recorded))
	for key := range recorded {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	var models []SessionConfigChoice
	for _, key := range keys {
		models = append(models, recorded[key]...)
	}
	stale := recorded["claude"][1]
	stale.Name = "Sonnet 5"
	models = append(models, stale)
	// Keep the winning moving alias even when a different spelling of
	// its identity appears before both old and new aliases.
	mixed := []SessionConfigChoice{recorded["antigravity"][2], stale, recorded["claude"][1]}
	if got := ModelID(MergedChoices(mixed), "sonnet"); got != "claude-sonnet-5-5" {
		t.Fatalf("mixed: %q", got)
	}
	for _, reverse := range []bool{false, true} {
		if reverse {
			slices.Reverse(models)
		}
		if got := ModelID(models, "sonnet"); got != "claude-sonnet-5-5" {
			t.Fatalf("reverse=%v: %q", reverse, got)
		}
		if _, ok := Offer([]SessionConfigChoice{stale}, "claude-sonnet-5-5", "sonnet"); ok {
			t.Fatal("stale offered")
		}
		if offer, ok := ModelCatalogResolve(recorded["claude"], "claude-sonnet-5-5"); !ok || offer.Value != "sonnet" {
			t.Fatalf("claude resolve: %+v %v", offer, ok)
		}
		if offer, ok := ModelCatalogResolve(recorded["antigravity"], "sonnet"); !ok || offer.Value != "claude-sonnet-5-5" {
			t.Fatalf("antigravity resolve: %+v %v", offer, ok)
		}
		if got := ModelID(models, "claude-sonnet-5"); got != "claude-sonnet-5" {
			t.Fatalf("explicit: %q", got)
		}
	}
	context := SessionConfigChoice{Value: "sonnet[1m]", Name: "Sonnet 5.5 (1M context)"}
	if got := ChoiceModelID(&context); got != "claude-sonnet-5-5[1m]" {
		t.Fatalf("context: %q", got)
	}
	if _, ok := Offer([]SessionConfigChoice{context}, "claude-sonnet-5-5", "sonnet"); ok {
		t.Fatal("context offered")
	}
	if _, ok := ModelCatalogResolve(models, "claude-sonnet-5-5[1m]"); ok {
		t.Fatal("context resolved")
	}
	opaque := SessionConfigChoice{Value: "private-router-model", Name: "Sonnet 5.5"}
	if got := ChoiceModelID(&opaque); got != "private-router-model" {
		t.Fatalf("opaque: %q", got)
	}
	if _, ok := Offer([]SessionConfigChoice{opaque}, "claude-sonnet-5-5", "sonnet"); ok {
		t.Fatal("opaque offered")
	}
	if got := ModelID(models, "unknown-model"); got != "unknown-model" {
		t.Fatalf("unknown: %q", got)
	}
}
