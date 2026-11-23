// Port of the tests in mj-tui/src/ui_requests.rs.

package tui

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

func uiRequestsBellOff(id uint64) client.UiRequest {
	value, err := json.Marshal(!config().Notify.Bell)
	if err != nil {
		panic(err)
	}
	return client.UiRequest{
		Incarnation: "daemon",
		ID:          id,
		Action: client.UiActionStageSettings{
			Edits: []client.SettingEdit{{Path: []string{"notify", "bell"}, Value: value}},
		},
	}
}

// uiRequestsConfigJSON is the dashboard's configuration as JSON, a deep copy
// to compare with later.
func uiRequestsConfigJSON(t *testing.T, dashboard *DashboardState) string {
	t.Helper()
	encoded, err := json.Marshal(*dashboard.config.Get())
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// Agents hold the API token and must not write the person's configuration:
// a staged edit opens Settings unsaved, on its page.
func TestStagedSettingsOpenUnsavedOnThePageThatHoldsThem(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	saved := uiRequestsConfigJSON(t, dashboard)

	dashboard.ApplyUIRequest(uiRequestsBellOff(1))

	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatalf("Settings did not open: %#v", dashboard.mode)
	}
	if page := setup.page(); !reflect.DeepEqual(page, []string{"notify"}) {
		t.Fatalf("page = %#v, want [notify]", page)
	}
	if !setup.isDirty() {
		t.Fatal("the staged edit is not a draft change")
	}
	if config := uiRequestsConfigJSON(t, dashboard); config != saved {
		t.Fatalf("the configuration changed:\n%s\nwant\n%s", config, saved)
	}
}

func TestARequestMadeBehindAnOpenDialogAppliesOnceTheDialogCloses(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	openNewSessionWizard(dashboard)
	wizard := reflect.TypeOf(dashboard.mode)

	dashboard.ApplyUIRequest(uiRequestsBellOff(1))
	if reflect.TypeOf(dashboard.mode) != wizard {
		t.Fatalf("mode = %#v, want the wizard still open", dashboard.mode)
	}
	if followup := dashboard.TakeUIFollowup(); followup != nil {
		t.Fatalf("followup = %#v while the wizard is open", followup)
	}

	dashboard.CancelModal()
	if followup := dashboard.TakeUIFollowup(); followup == nil {
		t.Fatal("the held request did not apply once the wizard closed")
	}
	if _, ok := dashboard.mode.(ModeSetup); !ok {
		t.Fatalf("mode = %#v, want Settings", dashboard.mode)
	}
	if followup := dashboard.TakeUIFollowup(); followup != nil {
		t.Fatalf("followup = %#v after the held request applied", followup)
	}
}
