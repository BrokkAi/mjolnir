// Port of the tests in mj-client/src/settings/page.rs, save.rs, schema.rs
// and view.rs.

package client

import (
	"encoding/json"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestSubagentsPageExposesFourCategoryLists(t *testing.T) {
	value := settingsJSON(t, `{"subagents": {}}`)
	path := []string{}
	Expand(&value, &path)
	subagents := settingsGet(value, "subagents")
	paths := VisiblePaths([]string{"subagents"}, subagents)
	want := [][]string{
		{"subagents", "max_concurrent"},
		{"subagents", "primary"},
		{"subagents", "straightforward"},
		{"subagents", "subtle"},
		{"subagents", "oracle"},
	}
	if !reflect.DeepEqual(paths, want) {
		t.Fatalf("paths %v", paths)
	}
}

// A client older than the daemon sends neither side of a settings edit with
// the fields it does not know. Those must neither conflict with nor replace
// what the file holds.
// Hard-won: #1557: once older clients could attach, their saves conflicted on fields newer than them
func TestASaveFromAClientThatPredatesAFieldKeepsTheFilesValue(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	current := core.DefaultConfig()
	var reviewer core.SubagentChoice
	if err := json.Unmarshal([]byte(`{"model": "reviewer-model"}`), &reviewer); err != nil {
		t.Fatal(err)
	}
	current.Review.Reviewers = []core.SubagentChoice{reviewer}
	if err := current.SaveTo(path); err != nil {
		t.Fatal(err)
	}
	// `review.reviewers` is always written, so a client that predates it is
	// the only way it can be missing.
	original, err := settingsToValue(current)
	if err != nil {
		t.Fatal(err)
	}
	review := settingsGet(original, "review").(jsonvalue.Object)
	review.SwapDelete("reviewers")
	settingsSet(&original, review, "review")
	updated := settingsClone(original)
	settingsSet(&updated, "right", "sessions_side")
	state := core.NewStateState()
	saved, err := SaveAt(path, settingsDisplay(original), settingsDisplay(updated), &state)
	if err != nil {
		t.Fatal(err)
	}
	if saved.SessionsSide != core.SessionsSideRight {
		t.Fatalf("sessions side %v", saved.SessionsSide)
	}
	loaded, err := core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(loaded.Review.Reviewers) != 1 || loaded.Review.Reviewers[0].Model != "reviewer-model" {
		t.Fatalf("reviewers %+v", loaded.Review.Reviewers)
	}
}

// Reviewer entries are SubagentChoice tables, which reject unknown keys, so
// expanding a draft must leave them exactly as stored.
func TestExpandingADraftLeavesReviewerEntriesUnchanged(t *testing.T) {
	reviewer := settingsJSON(t, `{"model": "gpt-6-terra", "effort": "xhigh"}`)
	draft := settingsJSON(t, `{"review": {"reviewers": [{"model": "gpt-6-terra", "effort": "xhigh"}]}}`)
	path := []string{}
	Expand(&draft, &path)
	if !settingsEqual(settingsPath(draft, "review", "reviewers"), []any{reviewer}) {
		t.Fatalf("reviewers %s", settingsDisplay(settingsPath(draft, "review", "reviewers")))
	}
}

func TestLegacyReviewDefaultsDoNotInsertReviewers(t *testing.T) {
	if got := Defaults([]string{"review"}, jsonvalue.Object{}); !settingsEqual(got, jsonvalue.Object{}) {
		t.Fatalf("defaults %s", settingsDisplay(got))
	}
}

func TestReviewSummaryIsOffWithoutReviewersAndCountsThemOtherwise(t *testing.T) {
	theme := func(_, _ any) string { return "" }
	summary := func(review string) string {
		draft := settingsJSON(t, `{"review": `+review+`}`)
		text, ok := SectionSummary("review", draft, theme)
		if !ok {
			return "<none>"
		}
		return text
	}
	reviewer := `{"model": "gpt-6-terra", "effort": "xhigh"}`
	for _, check := range []struct{ review, want string }{
		{`{"reviewers": []}`, "Off"},
		{`{}`, "Off"},
		{`{"reviewers": [` + reviewer + `]}`, "Before push · 1 reviewer"},
		{`{"reviewers": [` + reviewer + `, ` + reviewer + `]}`, "Before push · 2 reviewers"},
		{`{"trigger": "push"}`, "Before push · automatic reviewers"},
	} {
		if got := summary(check.review); got != check.want {
			t.Errorf("summary(%s) = %q, want %q", check.review, got, check.want)
		}
	}
}

func TestReviewersCanBeEditedAndNoTriggerIsOffered(t *testing.T) {
	draft := settingsJSON(t, `{"review": {"trigger": "push", "reviewers": [{"model": "gpt-6-terra"}]}, "_reviewer_options": {"0": {"models": [{"value": "gpt-6-terra", "label": "Terra"}], "efforts": []}}}`)
	if readOnly([]string{"review", "reviewers"}, draft) {
		t.Error("the reviewer list is read only")
	}
	if readOnly([]string{"review", "reviewers", "0", "model"}, draft) {
		t.Error("a discovered reviewer's model is read only")
	}
	page, err := RenderPage(draft, []string{"review"})
	if err != nil {
		t.Fatal(err)
	}
	if page.Actions[0].ID != "review.autoconfigure" {
		t.Errorf("action %q", page.Actions[0].ID)
	}
	// The stored trigger is not a setting, so no row offers it.
	for _, group := range page.Groups {
		for _, row := range group.Rows {
			if len(row.Path) > 0 && row.Path[len(row.Path)-1] == "trigger" {
				t.Errorf("a row offers the trigger: %v", row.Path)
			}
		}
	}
}
