// Port of the tests in mj-client/src/ui_request.rs.

package client

import (
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func uiTestRequest(incarnation string, id uint64) UiRequest {
	return UiRequest{
		Incarnation: incarnation,
		ID:          id,
		Action:      UiActionPaneSize{Pane: UiPaneSessions, Size: core.PaneSizeMinimized},
	}
}

// A dashboard survives daemon restarts; the new daemon numbers its requests
// from one again and they must still be applied.
func TestRequestsFromARestartedDaemonApplyAlthoughTheirIdsRestart(t *testing.T) {
	cursor := UiRequestCursorAfter([]UiRequest{uiTestRequest("old", 1), uiTestRequest("old", 7)})
	if fresh := cursor.TakeNew([]UiRequest{uiTestRequest("old", 1), uiTestRequest("old", 7)}); len(fresh) != 0 {
		t.Fatalf("replayed %d requests", len(fresh))
	}
	afterRestart := []UiRequest{uiTestRequest("new", 1), uiTestRequest("new", 2)}
	var ids []uint64
	for _, request := range cursor.TakeNew(afterRestart) {
		ids = append(ids, request.ID)
	}
	if !reflect.DeepEqual(ids, []uint64{1, 2}) {
		t.Fatalf("ids %v", ids)
	}
	if fresh := cursor.TakeNew(afterRestart); len(fresh) != 0 {
		t.Fatalf("replayed %d requests", len(fresh))
	}
}
