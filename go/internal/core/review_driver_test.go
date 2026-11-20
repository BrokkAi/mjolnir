// Port of the tests in mj-core/src/review/driver.rs.

package core

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func reviewDriverDecode(t *testing.T, value any) ReviewDriverSnapshot {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var snapshot ReviewDriverSnapshot
	if err := json.Unmarshal(encoded, &snapshot); err != nil {
		t.Fatalf("decode %s: %v", encoded, err)
	}
	return snapshot
}

func reviewDriverValue(t *testing.T, snapshot ReviewDriverSnapshot) jsonvalue.Object {
	t.Helper()
	encoded, err := json.Marshal(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	return value.(jsonvalue.Object)
}

func reviewDriverDecodedWithBrief(t *testing.T, snapshot jsonvalue.Object) ReviewDriverSnapshot {
	t.Helper()
	snapshot = snapshot.Clone()
	seed, _ := snapshot.Get("seed")
	seedObject := seed.(jsonvalue.Object).Clone()
	seedObject.Set("brief", "relations")
	snapshot.Set("seed", seedObject)
	decoded := reviewDriverDecode(t, snapshot)
	again := reviewDriverDecode(t, reviewDriverValue(t, decoded))
	if !reflect.DeepEqual(decoded, again) {
		t.Fatalf("a brief survives a save and restore: %+v != %+v", decoded, again)
	}
	return decoded
}

func TestOldPushSnapshotsSkipRetiredReplayAndKeepLiveRequestsStrict(t *testing.T) {
	stored, err := jsonvalue.Parse([]byte(`{
		"review_id":"push-1", "seed":{"task":"review commits","user_messages":[],"baselines":{},"through_ordinal":12},
		"phase":{"phase":"capturing_delta"}, "last_verdict":null,
		"deltas":[], "roles":[], "provenance":"single_reviewer", "sequence":0,
		"replay_requests":[{"CaptureDelta":{"baselines":{}}},{"AdvanceBaseline":{"trees":{},"reviewed_through_ordinal":12}},"ClearPriorReview",{"PromptPrimary":{"command_id":"old","prompt":"old"}},"Close"]
	}`))
	if err != nil {
		t.Fatal(err)
	}
	decoded := reviewDriverDecode(t, stored)
	if decoded.Seed.Brief != nil {
		t.Fatal("an older snapshot has no brief")
	}
	if want := []ReviewRequest{ReviewRequestCaptureDelta{}, ReviewRequestClose{}}; !reflect.DeepEqual(decoded.ReplayRequests, want) {
		t.Fatalf("replay requests = %#v", decoded.ReplayRequests)
	}
	saved := reviewDriverValue(t, decoded)
	seed, _ := saved.Get("seed")
	baselines, _ := seed.(jsonvalue.Object).Get("baselines")
	if !jsonvalue.Equal(baselines, jsonvalue.Object{}) {
		t.Fatalf("baselines = %v", baselines)
	}
	if through, _ := seed.(jsonvalue.Object).Get("through_ordinal"); !jsonvalue.Equal(through, int64(0)) {
		t.Fatalf("through_ordinal = %v", through)
	}
	if provenance, _ := saved.Get("provenance"); provenance != "independent_reviewers" {
		t.Fatalf("provenance = %v", provenance)
	}
	if _, present := seed.(jsonvalue.Object).Get("brief"); present {
		t.Fatal("an absent brief is written")
	}
	briefed := reviewDriverDecodedWithBrief(t, saved)
	if briefed.Seed.Brief == nil || *briefed.Seed.Brief != "relations" {
		t.Fatalf("brief = %v", briefed.Seed.Brief)
	}
	replay, _ := saved.Get("replay_requests")
	first := replay.([]any)[0]
	want, _ := jsonvalue.Parse([]byte(`{"CaptureDelta":{"baselines":{}}}`))
	if !jsonvalue.Equal(first, want) {
		t.Fatalf("first replay request = %v", first)
	}
	strict := stored.(jsonvalue.Object).Clone()
	strict.Set("replay_requests", []any{jsonvalue.Object{{Key: "StartReviewer", Value: jsonvalue.Object{}}}})
	encoded, err := jsonvalue.Marshal(strict)
	if err != nil {
		t.Fatal(err)
	}
	var refused ReviewDriverSnapshot
	if err := json.Unmarshal(encoded, &refused); err == nil {
		t.Fatal("a live request with missing fields was accepted")
	}
}
