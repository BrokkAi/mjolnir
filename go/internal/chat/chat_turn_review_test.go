// Port of mj-chat/src/chat/turn_review.rs (tests).

package chat

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 5fc728617: a writer permission form was hidden until reviewers started.
func TestTurnReviewSurfacesWriterQuestionsAndPermissionsWhileWriting(t *testing.T) {
	var view client.RuntimeReviewView
	if err := json.Unmarshal([]byte(`{
		"session_id":"parent",
		"phase":{"phase":"writing_brief"},
		"roles":[{"role":"brief-writer","label":"Brief writer","state":"running","child_session_id":"writer-child"}],
		"status":"writing a relation brief",
		"verdict":null,
		"questions":[
			{"role":"writer-child","request":{"id":"permission","message":"Allow reading the repository?","fields":[],"tool_permission":{"skip":true}}},
			{"role":"writer-child","request":{"id":"question","message":"Which entry point?","fields":[]}},
			{"role":"unrelated-child","request":{"id":"stale","message":"Stale question","fields":[]}}
		]
	}`), &view); err != nil {
		t.Fatal(err)
	}
	review := newTurnReview(view)
	pending := review.pendingElicitations()
	type owned struct{ child, request string }
	var got []owned
	for _, question := range pending {
		got = append(got, owned{question.Role, question.Request.ID})
	}
	if want := []owned{{"writer-child", "permission"}, {"writer-child", "question"}}; !reflect.DeepEqual(got, want) {
		t.Fatalf("pending = %v, want %v", got, want)
	}
	if pending[0].Request.ToolPermission == nil {
		t.Fatal("the writer's permission form lost its tool permission")
	}
	review.view.Phase = core.TurnReviewPhaseRunning{Roles: []core.RoleStatus{}}
	if again := review.pendingElicitations(); !reflect.DeepEqual(again, pending) {
		t.Fatalf("running pending = %#v, want %#v", again, pending)
	}
	review.view.Phase = core.TurnReviewPhaseResolved{Resolution: core.ResolutionCancelled}
	if resolved := review.pendingElicitations(); len(resolved) != 0 {
		t.Fatalf("resolved pending = %#v, want none", resolved)
	}
}
