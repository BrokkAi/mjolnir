// Port of mj-worker/src/acp/grok_usage.rs: its tests module.

package worker

import (
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// acpFeaturesJSON parses JSON text as serde_json's json! builds a value.
func acpFeaturesJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("parse %s: %v", text, err)
	}
	return value
}

// acpFeaturesSet sets a member of a JSON object value in place
// (`value[key] = member`).
func acpFeaturesSet(t *testing.T, value any, member any, path ...string) any {
	t.Helper()
	object, ok := value.(jsonvalue.Object)
	if !ok {
		t.Fatalf("not an object: %v", value)
	}
	object = object.Clone()
	if len(path) == 1 {
		object.Set(path[0], member)
		return object
	}
	inner, _ := object.Get(path[0])
	object.Set(path[0], acpFeaturesSet(t, inner, member, path[1:]...))
	return object
}

func grokUsageFixture(t *testing.T) any {
	return acpFeaturesJSON(t, `{"inputTokens":100,"outputTokens":30,"totalTokens":130,"cachedReadTokens":40,
            "cacheCreationTokens":10,"reasoningTokens":20,"modelCalls":2,"apiDurationMs":1700,"costUsdTicks":88767200,
            "modelUsage":{"grok-4.6-build":{"inputTokens":100,"outputTokens":30,"cachedReadTokens":40,"reasoningTokens":20,"costUsdTicks":88767200}}}`)
}

func TestGrokUsageKeepsZeroMissingAndIncompleteReportsDistinct(t *testing.T) {
	minimal, err := parse(acpFeaturesJSON(t, `{"inputTokens":0,"outputTokens":0}`), nil)
	if err != nil {
		t.Fatal(err)
	}
	if minimal.TotalTokens != 0 {
		t.Fatalf("total %d", minimal.TotalTokens)
	}
	if minimal.CachedReadTokens != nil {
		t.Fatalf("cached read %v", *minimal.CachedReadTokens)
	}
	if minimal.ProviderDetails.Cost != nil {
		t.Fatal("a report without cost has a cost")
	}
	partial := grokUsageFixture(t)
	partial = acpFeaturesSet(t, partial, true, "usageIsIncomplete")
	partial = acpFeaturesSet(t, partial, true, "costIsPartial")
	result, err := parse(partial, nil)
	if err != nil {
		t.Fatal(err)
	}
	if result.Scope != core.UsageScopeUnspecified {
		t.Fatalf("scope %v", result.Scope)
	}
	if !result.ProviderDetails.Cost.IsPartial {
		t.Fatal("partial cost is not partial")
	}
	if scope := result.ProviderDetails.ModelUsage["grok-4.6-build"].Scope; scope != core.UsageScopeUnspecified {
		t.Fatalf("model scope %v", scope)
	}
	for _, malformed := range []string{
		`{"outputTokens":1}`,
		`{"inputTokens":-1,"outputTokens":2}`,
		`{"inputTokens":18446744073709551615,"outputTokens":1}`,
		`{"inputTokens":1,"outputTokens":2,"totalTokens":4}`,
	} {
		if _, err := parse(acpFeaturesJSON(t, malformed), nil); err == nil {
			t.Fatalf("%s parsed", malformed)
		}
	}
}

func TestGrokUsageIsCorrelatedAndDuplicatesAreNotAdded(t *testing.T) {
	var pending PendingUsage
	update := acpFeaturesJSON(t, `{"sessionUpdate":"turn_completed","prompt_id":"p1"}`)
	update = acpFeaturesSet(t, update, grokUsageFixture(t), "usage")
	observe := func(session string, update any) bool {
		t.Helper()
		observed, err := pending.Observe(session, update)
		if err != nil {
			t.Fatal(err)
		}
		return observed
	}
	if observe("s", update) {
		t.Fatal("observed before the prompt began")
	}
	pending.Begin("s")
	if observe("foreign", update) {
		t.Fatal("observed a foreign session")
	}
	if !observe("s", update) || !observe("s", update) {
		t.Fatal("the live prompt's report was not observed")
	}
	id := "p1"
	if usage := pending.Finish(&id); usage == nil || usage.TotalTokens != 130 {
		t.Fatalf("finish %v", usage)
	}
	pending.Begin("s")
	observe("s", update)
	other := "p2"
	if usage := pending.Finish(&other); usage != nil {
		t.Fatalf("finish p2 %v", usage)
	}
	pending.Begin("s")
	observe("s", update)
	conflicting := acpFeaturesSet(t, update, int64(9000), "usage", "apiDurationMs")
	if _, err := pending.Observe("s", conflicting); err == nil {
		t.Fatal("a conflicting report was accepted")
	}
	if usage := pending.Finish(&id); usage != nil {
		t.Fatalf("finish after conflict %v", usage)
	}
}
