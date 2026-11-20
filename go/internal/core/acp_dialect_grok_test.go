// Port of the tests in mj-core/src/acp/dialect/grok.rs.

package core

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
)

const grokModelMetaJSON = `{"modelState": {
	"currentModelId": "grok-4.6",
	"availableModels": [
		{
			"modelId": "grok-4.6",
			"name": "Grok 4.6",
			"_meta": {
				"reasoningEffort": "high",
				"reasoningEfforts": [
					{"value": "xhigh", "label": "Extra High"},
					{"value": "high", "label": "High"},
					{"value": "medium", "label": "Medium"},
					{"value": "low", "label": "Low"}
				]
			}
		},
		{
			"modelId": "grok-4.5",
			"name": "Grok 4.5",
			"_meta": {
				"reasoningEffort": "high",
				"reasoningEfforts": [
					{"value": "high", "label": "High"},
					{"value": "low", "label": "Low"}
				]
			}
		}
	]
}}`

func grokMeta(t *testing.T, text string) schema.Meta {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("ACP metadata: %v", err)
	}
	meta, ok := value.(jsonvalue.Object)
	if !ok {
		t.Fatal("ACP metadata object")
	}
	return meta
}

// grokRequest is the request ACP's UntypedMessage envelope carries.
func grokRequest(t *testing.T, params []byte) any {
	t.Helper()
	parsed, err := jsonvalue.Parse(params)
	if err != nil {
		t.Fatal(err)
	}
	return jsonvalue.Object{{Key: "method", Value: grokSetModelMethod}, {Key: "params", Value: parsed}}
}

func TestRealShapedGrokModelChangeBuildsTheLegacyACPSetModelRequest(t *testing.T) {
	// Hand-authored from Grok's modelState extension and ACP's UntypedMessage envelope.
	fixture, err := os.ReadFile(filepath.Join("testdata", "acp_dialect_grok", "grok-model-state.json"))
	if err != nil {
		t.Fatal(err)
	}
	state := GrokModelStateOf(grokMeta(t, string(fixture)))
	if state == nil {
		t.Fatal("Grok model catalogue")
	}
	sessionID := schema.SessionId("grok-session-42")

	params, updated, err := GrokSetModelRequest(sessionID, state, "model", "grok-4.5")
	if err != nil {
		t.Fatalf("select the advertised model: %v", err)
	}
	want := grokMeta(t, `{"method": "session/set_model", "params": {"sessionId": "grok-session-42", "modelId": "grok-4.5"}}`)
	if got := grokRequest(t, params); !jsonvalue.Equal(got, want) {
		t.Fatalf("request = %s", params)
	}
	if updated.currentModelID != "grok-4.5" || updated.currentEffort == nil || *updated.currentEffort != "high" {
		t.Fatalf("state = %+v", updated)
	}

	params, updated, err = GrokSetModelRequest(sessionID, &updated, "effort", "low")
	if err != nil {
		t.Fatalf("select an advertised effort: %v", err)
	}
	want = grokMeta(t, `{"method": "session/set_model", "params": {"sessionId": "grok-session-42", "modelId": "grok-4.5", "_meta": {"reasoningEffort": "low"}}}`)
	if got := grokRequest(t, params); !jsonvalue.Equal(got, want) {
		t.Fatalf("request = %s", params)
	}
	if updated.currentEffort == nil || *updated.currentEffort != "low" {
		t.Fatalf("state = %+v", updated)
	}
}

func TestConfigOptionsDoNotInventAnUnknownEffort(t *testing.T) {
	meta := grokMeta(t, strings.Replace(grokModelMetaJSON, `"reasoningEffort": "high",`, "", 1))

	options := GrokConfigOptions(GrokModelStateOf(meta))

	if FindSessionConfigOption(options, "model") == nil {
		t.Fatal("no model option")
	}
	if FindSessionConfigOption(options, "effort") != nil {
		t.Fatal("an unknown effort was invented")
	}
}

func TestRealConfigOptionsSurviveSynthesizedModelRefreshes(t *testing.T) {
	state := GrokModelStateOf(grokMeta(t, grokModelMetaJSON))
	options := []schema.SessionConfigOption{{
		Kind: schema.SessionConfigOptionKindSelect,
		ID:   "verbosity",
		Name: "Verbosity",
		Select: &schema.SessionConfigSelect{
			CurrentValue: "normal",
			Options:      []schema.SessionConfigSelectOption{{Value: "normal", Name: "Normal"}},
		},
	}}

	GrokMergeConfigOptions(&options, state)

	for _, key := range []string{"verbosity", "model", "effort"} {
		if FindSessionConfigOption(options, key) == nil {
			t.Fatalf("no %s option", key)
		}
	}
}

func TestResponseLossIsDistinctFromAnAgentsExplicitRejection(t *testing.T) {
	lost := &acp.RPCError{Code: -32603, Message: "response to `session/set_model` never received: channel closed"}
	rejected := &acp.RPCError{Code: -32602, Message: "Invalid params"}

	if !GrokResponseWasLost(lost) {
		t.Fatal("a lost response is not recognized")
	}
	if GrokResponseWasLost(rejected) {
		t.Fatal("a rejection reads as a lost response")
	}
}
