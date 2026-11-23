// Port of mj-tui/src/ui_requests.rs.
//
// Applying dashboard requests an agent made on the person's behalf (`mj ui`).
//
// A request is input, like a keypress: it goes through the same methods and
// DashboardActions the person's own keys use, so it can only do what they
// could. One that would cover or replace an open dialog waits until the
// dialog closes, rather than discarding what the person was doing.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// heldUIRequests is how many requests may wait behind an open dialog. Older
// ones are dropped first: the newest is what the person asked for last.
const heldUIRequests = 8

// ApplyUIRequest applies one request now, or holds it behind the open dialog
// and the requests already waiting, so requests always apply in the order
// made.
func (d *DashboardState) ApplyUIRequest(request client.UiRequest) DashboardAction {
	panic("unported: mj-tui/src/ui_requests.rs DashboardState::apply_ui_request")
}

// TakeUIFollowup applies the oldest waiting request, once it can apply; nil
// when none can.
func (d *DashboardState) TakeUIFollowup() DashboardAction {
	if len(d.heldUIRequests) == 0 || !d.uiRequestReady(d.heldUIRequests[0].Action) {
		return nil
	}
	request := d.heldUIRequests[0]
	d.heldUIRequests = d.heldUIRequests[1:]
	return d.runUIRequest(&request)
}

// uiRequestReady reports whether action can apply without covering or
// replacing something the person has open. Staged settings join an open
// Settings unless the person is typing a value, searching, or saving there.
func (d *DashboardState) uiRequestReady(action client.UiAction) bool {
	panic("unported: mj-tui/src/ui_requests.rs DashboardState::ui_request_ready")
}

func (d *DashboardState) runUIRequest(request *client.UiRequest) DashboardAction {
	panic("unported: mj-tui/src/ui_requests.rs DashboardState::run_ui_request")
}

// uiRequester is who asked, as the notice names them.
func (d *DashboardState) uiRequester(request *client.UiRequest) string {
	panic("unported: mj-tui/src/ui_requests.rs DashboardState::ui_requester")
}

func (d *DashboardState) stageRequestedSettings(request *client.UiRequest) DashboardAction {
	panic("unported: mj-tui/src/ui_requests.rs DashboardState::stage_requested_settings")
}
