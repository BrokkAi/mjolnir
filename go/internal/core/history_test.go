// Port of the tests in mj-core/src/history.rs.

package core

import "testing"

func TestWorkersWithContextClearButWithoutHistoryDoNotReceiveHistoryQueries(t *testing.T) {
	negotiated, ok := RelayVersionRangeCurrent.Negotiate(RelayVersionRange{Min: 1, Max: 15})
	if !ok {
		t.Fatal("no common relay protocol")
	}
	if !RelayRequestSupportedAt(RelayRequestSubmit{CommandID: "clear", Command: RelayCommandClearContext{}}, negotiated) {
		t.Fatal("context clear is not supported")
	}
	if RelayRequestSupportedAt(RelayRequestHistoryRequests{}, negotiated) {
		t.Fatal("history requests reach an old worker")
	}
	if !RelayRequestSupportedAt(RelayRequestHistoryRequests{}, RelayProtocolVersion) {
		t.Fatal("history requests are not supported")
	}
}
