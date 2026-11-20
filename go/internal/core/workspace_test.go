// Port of the tests in mj-core/src/workspace.rs.

package core

import (
	"encoding/json"
	"testing"
)

func TestLegacyLayoutsDecodeWithoutInventingABrowseLocation(t *testing.T) {
	var layout ConversationLayout
	if err := json.Unmarshal([]byte(`{
		"root": {"kind": "pane", "id": 17}, "focus": 17, "sessions": {"17": "session"}
	}`), &layout); err != nil {
		t.Fatal(err)
	}
	if layout.Browse != nil {
		t.Fatalf("browse = %d", *layout.Browse)
	}
	if len(layout.Pins) != 0 {
		t.Fatalf("pins = %v", layout.Pins)
	}
	if err := layout.Validate(); err != nil {
		t.Fatal(err)
	}
}
