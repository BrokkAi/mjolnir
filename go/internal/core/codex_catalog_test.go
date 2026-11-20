// Port of the tests in mj-core/src/codex_catalog.rs.

package core

import (
	"encoding/json"
	"reflect"
	"slices"
	"testing"
)

// codexCatalogEfforts is the `effort` string of each entry in
// `supported_reasoning_levels`, accepting both the object shape
// (`{"effort": "low"}`) the backfill and Z.ai emit and the plain-string shape
// used in some fixtures.
func codexCatalogEfforts(entry map[string]any) []string {
	levels, _ := entry["supported_reasoning_levels"].([]any)
	efforts := []string{}
	for _, level := range levels {
		switch level := level.(type) {
		case string:
			efforts = append(efforts, level)
		case map[string]any:
			effort, _ := level["effort"].(string)
			efforts = append(efforts, effort)
		default:
			efforts = append(efforts, "")
		}
	}
	return efforts
}

// codexCatalogValueEqual compares a catalog value with a JSON literal, as
// serde_json's Value equality does.
func codexCatalogValueEqual(t *testing.T, got any, literal string) bool {
	t.Helper()
	var want any
	if err := json.Unmarshal([]byte(literal), &want); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(got)
	if err != nil {
		t.Fatal(err)
	}
	var normalized any
	if err := json.Unmarshal(encoded, &normalized); err != nil {
		t.Fatal(err)
	}
	return reflect.DeepEqual(normalized, want)
}

const codexCatalogBody = `{"models":[
        {"slug":"glm-5.3","supported_reasoning_levels":["low","high","max"],"display_name":"GLM 5.3"},
        {"slug":"glm-5.3-flash","supported_reasoning_levels":["low","high","max"]},
        {"slug":"glm-5-turbo","some_future_field":{"nested":true}}
    ]}`

func TestParseKeepsUnknownFieldsAndReportsSlugs(t *testing.T) {
	catalog, err := CodexCatalogParse([]byte(codexCatalogBody))
	if err != nil {
		t.Fatal(err)
	}
	if got := catalog.Slugs(); !slices.Equal(got, []string{"glm-5.3", "glm-5.3-flash", "glm-5-turbo"}) {
		t.Fatalf("slugs %q", got)
	}
	if !codexCatalogValueEqual(t, catalog.Models[2]["some_future_field"].(map[string]any)["nested"], "true") {
		t.Fatal("nested field lost")
	}
	if !codexCatalogValueEqual(t, catalog.Models[0]["display_name"], `"GLM 5.3"`) {
		t.Fatal("unread fields survive a round trip")
	}
}

const codexCatalogOpenAIList = `{"object":"list","data":[
        {"id":"deepseek-flash","object":"model","owned_by":"deepseek"},
        {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
    ]}`

// Hard-won: the live deepseek-flash alias fell back to 128k and compacted far before the intended 800k threshold.
func TestParseTranslatesAnOpenAIModelListIntoCatalogEntries(t *testing.T) {
	catalog, err := CodexCatalogParse([]byte(codexCatalogOpenAIList))
	if err != nil {
		t.Fatal(err)
	}
	if got := catalog.Slugs(); !slices.Equal(got, []string{"deepseek-flash", "deepseek-v4-pro"}) {
		t.Fatalf("slugs %q", got)
	}
	flash := catalog.Models[0]
	check := func(value any, literal, message string) {
		t.Helper()
		if !codexCatalogValueEqual(t, value, literal) {
			t.Errorf("%s: got %v, want %s", message, value, literal)
		}
	}
	check(flash["display_name"], `"deepseek-flash"`, "display name")
	check(flash["description"], `"deepseek-flash (deepseek)"`, "the owner explains where the model came from")
	if got := codexCatalogEfforts(flash); !slices.Equal(got, []string{"low", "high", "max"}) {
		t.Errorf("a known family's levels are backfilled although the list omits them: %q", got)
	}
	check(flash["default_reasoning_level"], `"high"`, "the backfilled family default is supplied")
	check(flash["context_window"], `1000000`, "context window")
	check(flash["max_context_window"], `1000000`, "max context window")
	check(flash["auto_compact_token_limit"], `800000`, "the provider's Flash alias uses the DeepSeek V4 limits")
	pro := catalog.Models[1]
	check(pro["context_window"], `1000000`, "pro context window")
	check(pro["max_context_window"], `1000000`, "pro max context window")
	check(pro["auto_compact_token_limit"], `800000`, "DeepSeek V4 compacts where deepseek-harness does")
	check(flash["priority"], `0`, "flash priority")
	check(catalog.Models[1]["priority"], `1`, "pro priority")
	check(flash["truncation_policy"].(map[string]any)["limit"], `10000`, "truncation limit")
	// Codex must be able to read back what the translation produced.
	if _, err := CodexCatalogParse([]byte(catalog.ToJSON())); err != nil {
		t.Fatal(err)
	}
}

func TestBackfillFillsACodexEntryThatListsNoReasoningLevels(t *testing.T) {
	// Z.ai returns `glm-5-turbo` with a `max` default but no levels, which
	// would otherwise leave the model with no effort selector.
	catalog, err := CodexCatalogParse([]byte(`{"models":[
                {"slug":"glm-5.3","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},
                {"slug":"glm-5-turbo","default_reasoning_level":"max"}
            ]}`))
	if err != nil {
		t.Fatal(err)
	}
	if got := codexCatalogEfforts(catalog.Models[0]); !slices.Equal(got, []string{"low", "high"}) {
		t.Errorf("a provider that states its levels keeps exactly those: %q", got)
	}
	if got := codexCatalogEfforts(catalog.Models[1]); !slices.Equal(got, []string{"low", "high", "max"}) {
		t.Errorf("an empty list is backfilled for a known family: %q", got)
	}
	if !codexCatalogValueEqual(t, catalog.Models[1]["default_reasoning_level"], `"max"`) {
		t.Error("a default the provider already stated is preserved")
	}
}

func TestBackfillLeavesAnUnknownFamilyWithoutReasoningLevels(t *testing.T) {
	catalog, err := CodexCatalogParse([]byte(`{"object":"list","data":[{"id":"mystery-1","object":"model"}]}`))
	if err != nil {
		t.Fatal(err)
	}
	if got := codexCatalogEfforts(catalog.Models[0]); len(got) != 0 {
		t.Errorf("an unknown model still offers no effort rather than a guessed one: %q", got)
	}
	if _, ok := catalog.Models[0]["default_reasoning_level"]; ok {
		t.Error("and no default is invented for it")
	}
}

func TestOverridesRefineAListedModelAndAppendAnUnlistedOne(t *testing.T) {
	catalog, err := CodexCatalogParse([]byte(codexCatalogOpenAIList))
	if err != nil {
		t.Fatal(err)
	}
	overrides, err := ParseCodexShape([]byte(`{"models":[
                {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high"],"context_window":256000},
                {"slug":"deepseek-reasoner","display_name":"DeepSeek Reasoner"}
            ]}`))
	if err != nil {
		t.Fatal(err)
	}

	MergeOverrides(&catalog, &overrides)

	if got := catalog.Slugs(); !slices.Equal(got, []string{"deepseek-flash", "deepseek-v4-pro", "deepseek-reasoner"}) {
		t.Fatalf("an unlisted slug is appended in override order: %q", got)
	}
	check := func(value any, literal, message string) {
		t.Helper()
		if !codexCatalogValueEqual(t, value, literal) {
			t.Errorf("%s: got %v, want %s", message, value, literal)
		}
	}
	pro := catalog.Models[1]
	check(pro["supported_reasoning_levels"], `["low", "high"]`, "the override field replaces the translated default")
	check(pro["context_window"], `256000`, "context window")
	check(pro["display_name"], `"deepseek-v4-pro"`, "fields the override omits survive")
	if got := codexCatalogEfforts(catalog.Models[0]); !slices.Equal(got, []string{"low", "high", "max"}) {
		t.Errorf("a model the override does not name keeps its backfilled levels: %q", got)
	}
	// An appended slug is completed with the conservative defaults a
	// translated entry gets, so Codex accepts it rather than rejecting the
	// catalog for missing required fields and failing the launch.
	appended := catalog.Models[2]
	check(appended["display_name"], `"DeepSeek Reasoner"`, "the override field is kept on an appended entry")
	check(appended["context_window"], `1000000`, "appended context window")
	check(appended["auto_compact_token_limit"], `800000`, "appended compaction")
	check(appended["shell_type"], `"shell_command"`, "appended shell type")
	if _, ok := appended["supported_reasoning_levels"]; !ok {
		t.Error("an appended entry carries the default fields Codex requires")
	}
}

func TestGuardianReviewerComparesVersionsNumericallyNotAsText(t *testing.T) {
	picked, ok := GuardianReviewModel([]string{"glm-5.3-flash", "glm-5.10-flash"})
	if !ok || picked != "glm-5.10-flash" {
		t.Fatalf("picked %q %v", picked, ok)
	}
}

func TestStampingTheReviewerMarksEveryEntryAndSurvivesSerialization(t *testing.T) {
	catalog, err := CodexCatalogParse([]byte(codexCatalogBody))
	if err != nil {
		t.Fatal(err)
	}
	reviewer, ok := GuardianReviewModel(catalog.Slugs())
	if !ok {
		t.Fatal("flash model")
	}
	StampReviewer(&catalog, reviewer)
	restaged, err := CodexCatalogParse([]byte(catalog.ToJSON()))
	if err != nil {
		t.Fatal(err)
	}
	for _, model := range restaged.Models {
		if model["auto_review_model_override"] != reviewer {
			t.Fatalf("override %v", model["auto_review_model_override"])
		}
	}
	if restaged.Models[2]["some_future_field"].(map[string]any)["nested"] != true {
		t.Fatal("nested field lost")
	}
}
