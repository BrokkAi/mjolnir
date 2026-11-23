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
	"bytes"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// heldUIRequests is how many requests may wait behind an open dialog. Older
// ones are dropped first: the newest is what the person asked for last.
const heldUIRequests = 8

// ApplyUIRequest applies one request now, or holds it behind the open dialog
// and the requests already waiting, so requests always apply in the order
// made.
func (d *DashboardState) ApplyUIRequest(request client.UiRequest) DashboardAction {
	// A pane size touches nothing the person is working in, so it never
	// waits.
	_, paneSize := request.Action.(client.UiActionPaneSize)
	waits := !paneSize && (len(d.heldUIRequests) > 0 || !d.uiRequestReady(request.Action))
	if !waits {
		return d.runUIRequest(&request)
	}
	if d.ModalOpen() {
		who := d.uiRequester(&request)
		d.SetNotice(who + " asked to change the dashboard; that happens when this dialog closes.")
	}
	d.heldUIRequests = append(d.heldUIRequests, request)
	for len(d.heldUIRequests) > heldUIRequests {
		d.heldUIRequests = d.heldUIRequests[1:]
	}
	return DashboardActionNone{}
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
	switch action.(type) {
	case client.UiActionPaneSize:
		return true
	case client.UiActionStageSettings:
		if setup, ok := d.mode.(ModeSetup); ok {
			return setup.acceptsStagedEdits() && d.awayDialog() == nil
		}
		return !d.ModalOpen()
	default:
		return !d.ModalOpen()
	}
}

func (d *DashboardState) runUIRequest(request *client.UiRequest) DashboardAction {
	switch action := request.Action.(type) {
	case client.UiActionPaneSize:
		var pane SupportPane
		var label string
		switch action.Pane {
		case client.UiPaneSessions:
			pane, label = SupportPaneSessions, "Sessions"
		case client.UiPaneTargets:
			pane, label = SupportPaneTargets, "Targets"
		default:
			pane, label = SupportPaneQuota, "Profiles"
		}
		var sizeLabel string
		switch action.Size {
		case core.PaneSizeMinimized:
			sizeLabel = "minimized"
		case core.PaneSizeStandard:
			sizeLabel = "standard"
		default:
			sizeLabel = "maximized"
		}
		d.SetPaneSize(pane, action.Size)
		who := d.uiRequester(request)
		d.SetNotice(fmt.Sprintf("%s set the %s pane to %s.", who, label, sizeLabel))
		return DashboardActionNone{}
	case client.UiActionStageSettings:
		return d.stageRequestedSettings(request)
	case client.UiActionShowSession:
		session := d.state.Get().Sessions.GetShared(action.SessionID)
		if session == nil {
			who := d.uiRequester(request)
			d.SetNotice(fmt.Sprintf(
				"%s asked to show session %s, which this dashboard does not list.", who, action.SessionID,
			))
			return DashboardActionNone{}
		}
		workspaceID := session.WorkspaceID
		var direction chat.Direction
		split := true
		switch action.Placement {
		case client.SessionPlacementRight:
			direction = chat.DirectionHorizontal
		case client.SessionPlacementBelow:
			direction = chat.DirectionVertical
		default:
			split = false
		}
		// Panes belong to a workspace; a session in another one opens
		// in its own workspace's conversation instead.
		if active := *d.activeWorkspaceID.Get(); split && active != nil && *active == workspaceID {
			return DashboardActionOpenSessionInSplit{SessionID: action.SessionID, Direction: direction}
		}
		return d.focusSessionAnywhere(workspaceID, action.SessionID)
	}
	return DashboardActionNone{}
}

// uiRequester is who asked, as the notice names them.
func (d *DashboardState) uiRequester(request *client.UiRequest) string {
	if request.FromSessionID != nil {
		if session := d.state.Get().Sessions.GetShared(*request.FromSessionID); session != nil {
			return "The agent in " + sessionName(session)
		}
	}
	return "An agent"
}

func (d *DashboardState) stageRequestedSettings(request *client.UiRequest) DashboardAction {
	action, ok := request.Action.(client.UiActionStageSettings)
	if !ok {
		return DashboardActionNone{}
	}
	who := d.uiRequester(request)
	listed := make([]string, 0, len(action.Edits))
	for _, edit := range action.Edits {
		listed = append(listed, edit.Dotted()+" = "+uiRequestValueText(edit.Value))
	}
	notice := fmt.Sprintf(
		"%s staged %s. Review and press Save to keep, or Esc to discard.", who, strings.Join(listed, ", "),
	)
	if err := d.beginStagedSettings(action.Edits, notice); err != nil {
		d.SetNotice(fmt.Sprintf("%s asked to change settings, but %v", who, err))
	}
	return DashboardActionNone{}
}

// uiRequestValueText is serde_json::Value's Display: compact JSON with object
// keys sorted, as serde_json's default map keeps them, and `<`, `>` and `&`
// left literal.
func uiRequestValueText(value json.RawMessage) string {
	decoder := json.NewDecoder(bytes.NewReader(value))
	decoder.UseNumber()
	var parsed any
	if err := decoder.Decode(&parsed); err != nil {
		return string(value)
	}
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(parsed); err != nil {
		return string(value)
	}
	return strings.TrimSuffix(buffer.String(), "\n")
}
