// Port of mj-worker/src/acp/verdict_client/glide_turn.rs: its test-only
// items and tests.

package worker

import (
	"os"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// testSamples are mj-core's recorded GLiDE answers (a JSON array), each
// given an attention answer: these recorded vectors predate attention.
// Exercise the new axis without altering the frozen action probabilities or
// their recorded provenance. The samples are read from
// ../core/testdata/activity_eot_glide/eot_glide_fresh_sample.json.
func testSamples() any {
	data, err := os.ReadFile("../core/testdata/activity_eot_glide/eot_glide_fresh_sample.json")
	if err != nil {
		panic(err)
	}
	parsed, err := jsonvalue.Parse(data)
	if err != nil {
		panic(err)
	}
	samples := parsed.([]any)
	for index, sample := range samples {
		object := sample.(jsonvalue.Object)
		answers, _ := object.Get("answers")
		answersObject := answers.(jsonvalue.Object)
		answersObject.Set("attention", jsonvalue.Object{{Key: "probabilities", Value: jsonvalue.Object{
			{Key: "needs_input", Value: 0.0},
			{Key: "important", Value: 1.0},
			{Key: "redundant_request", Value: 0.0},
			{Key: "none", Value: 0.0},
			{Key: "unclear", Value: 0.0},
		}}})
		object.Set("answers", answersObject)
		samples[index] = object
	}
	return samples
}

// glideTurnEdit returns a deep copy of sample with edit applied to the
// object at path.
func glideTurnEdit(sample any, edit func(object *jsonvalue.Object), path ...string) any {
	copied := sample.(jsonvalue.Object).Clone()
	var walk func(object jsonvalue.Object, path []string) jsonvalue.Object
	walk = func(object jsonvalue.Object, path []string) jsonvalue.Object {
		if len(path) == 0 {
			edit(&object)
			return object
		}
		inner, _ := object.Get(path[0])
		object.Set(path[0], walk(inner.(jsonvalue.Object), path[1:]))
		return object
	}
	return walk(copied, path)
}

func TestGlideDistributionsValidateRealProviderVectorsAndRejectPartialScores(t *testing.T) {
	for _, sample := range testSamples().([]any) {
		if _, err := core.ProbabilitiesParse(sample); err != nil {
			t.Fatalf("recorded sample: %v", err)
		}
		partial := glideTurnEdit(sample, func(object *jsonvalue.Object) { object.SwapDelete("required") },
			"answers", "input", "probabilities")
		if _, err := core.ProbabilitiesParse(partial); err == nil {
			t.Fatal("a partial distribution parsed")
		}
		invalid := glideTurnEdit(sample, func(object *jsonvalue.Object) { object.Set("finished", 2.0) },
			"answers", "turn_state", "probabilities")
		if _, err := core.ProbabilitiesParse(invalid); err == nil {
			t.Fatal("an invalid probability parsed")
		}
		olderProxy := glideTurnEdit(sample, func(object *jsonvalue.Object) { object.SwapDelete("attention") }, "answers")
		// Hard-won: 4dbf5e62d: a proxy without the attention question failed every verdict.
		parsed, err := core.ProbabilitiesParse(olderProxy)
		if err != nil {
			t.Fatalf("older proxy: %v", err)
		}
		if parsed.Attention != nil {
			t.Fatal("an older proxy has an attention answer")
		}
		partialAttention := glideTurnEdit(sample, func(object *jsonvalue.Object) { object.SwapDelete("important") },
			"answers", "attention", "probabilities")
		if _, err := core.ProbabilitiesParse(partialAttention); err == nil {
			t.Fatal("a partial attention answer parsed")
		}
	}
}
