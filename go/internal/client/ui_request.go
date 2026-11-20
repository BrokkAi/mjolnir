// Port of mj-client/src/ui_request.rs.

// Requests to change what the person's dashboard shows, made on their behalf.
//
// An agent asked to "maximize the Profiles pane" or "turn the bell off" sends
// one of these to the daemon, which checks it and publishes it in the runtime
// feed. Each attached dashboard applies it as input, through the same paths
// its own keys use, so a request can do only what a keypress could. Settings
// are staged in an unsaved Settings draft; the person saves them.

package client

import (
	"cmp"
	"encoding/json"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// UiPane is one of the dashboard's support panes, by the name the person
// sees.
type UiPane string

const (
	UiPaneSessions UiPane = "sessions"
	UiPaneTargets  UiPane = "targets"
	UiPaneProfiles UiPane = "profiles"
)

func (p *UiPane) UnmarshalText(text []byte) error {
	switch value := UiPane(text); value {
	case UiPaneSessions, UiPaneTargets, UiPaneProfiles:
		*p = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"sessions", "targets", "profiles"})
}

// SessionPlacement is where a shown session goes: the main conversation, or
// a new pane beside or below it.
type SessionPlacement string

const (
	SessionPlacementMain  SessionPlacement = "main"
	SessionPlacementRight SessionPlacement = "right"
	SessionPlacementBelow SessionPlacement = "below"
)

func (p *SessionPlacement) UnmarshalText(text []byte) error {
	switch value := SessionPlacement(text); value {
	case SessionPlacementMain, SessionPlacementRight, SessionPlacementBelow:
		*p = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"main", "right", "below"})
}

// SettingEdit is one setting to stage, by its Settings page path
// (`["notify", "bell"]`).
type SettingEdit struct {
	Path  []string        `json:"path"`
	Value json.RawMessage `json:"value"`
}

func (v SettingEdit) MarshalJSON() ([]byte, error) {
	type plain SettingEdit
	return daemonMarshalStruct(plain(v))
}

func (v *SettingEdit) UnmarshalJSON(data []byte) error {
	type plain SettingEdit
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// Dotted is the path as people and the configuration reference write it.
func (e SettingEdit) Dotted() string {
	return strings.Join(e.Path, ".")
}

// UiAction is what a request asks a dashboard to do. Internally tagged by
// "kind" in snake_case.
type UiAction interface {
	isUiAction()
}

type UiActionPaneSize struct {
	Pane UiPane        `json:"pane"`
	Size core.PaneSize `json:"size"`
}

type UiActionShowSession struct {
	SessionID string           `json:"session_id"`
	Placement SessionPlacement `json:"placement"`
}

// UiActionStageSettings opens Settings with these edits applied to an
// unsaved draft.
type UiActionStageSettings struct {
	Edits []SettingEdit `json:"edits"`
}

func (UiActionPaneSize) isUiAction()      {}
func (UiActionShowSession) isUiAction()   {}
func (UiActionStageSettings) isUiAction() {}

func (v UiActionPaneSize) MarshalJSON() ([]byte, error) {
	type plain UiActionPaneSize
	return daemonInternalJSON("kind", "pane_size", plain(v))
}

func (v UiActionShowSession) MarshalJSON() ([]byte, error) {
	type plain UiActionShowSession
	return daemonInternalJSON("kind", "show_session", plain(v))
}

func (v UiActionStageSettings) MarshalJSON() ([]byte, error) {
	type plain UiActionStageSettings
	return daemonInternalJSON("kind", "stage_settings", plain(v))
}

// UnmarshalUiActionJSON decodes a UiAction by its "kind" tag.
func UnmarshalUiActionJSON(data []byte) (UiAction, error) {
	name, err := daemonSplitInternal(data, "kind")
	if err != nil {
		return nil, err
	}
	switch name {
	case "pane_size":
		var v UiActionPaneSize
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	case "show_session":
		var v UiActionShowSession
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	case "stage_settings":
		var v UiActionStageSettings
		if err := daemonDecodeStruct(data, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"pane_size", "show_session", "stage_settings"})
}

// UiRequestBody is what a caller sends to `POST /api/v1/ui/requests`.
type UiRequestBody struct {
	// FromSessionID is the session whose agent asked, named in the
	// dashboard's notice.
	FromSessionID *string  `json:"from_session_id" serde:"default"`
	Action        UiAction `json:"action"`
}

func (v UiRequestBody) MarshalJSON() ([]byte, error) {
	type plain UiRequestBody
	return daemonMarshalStruct(plain(v))
}

func (v *UiRequestBody) UnmarshalJSON(data []byte) error {
	type plain UiRequestBody
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// UiRequest is a checked request as the runtime feed publishes it.
//
// ID increases within one daemon Incarnation; a dashboard that outlives a
// daemon restart sees a new incarnation and knows the numbering began again.
type UiRequest struct {
	Incarnation   string   `json:"incarnation"`
	ID            uint64   `json:"id"`
	FromSessionID *string  `json:"from_session_id" serde:"default"`
	Action        UiAction `json:"action"`
}

func (v UiRequest) MarshalJSON() ([]byte, error) {
	type plain UiRequest
	return daemonMarshalStruct(plain(v))
}

func (v *UiRequest) UnmarshalJSON(data []byte) error {
	type plain UiRequest
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// UiRequestReceipt is the daemon's answer: the request's id and how many
// dashboards were attached to receive it.
type UiRequestReceipt struct {
	ID         uint64 `json:"id"`
	Dashboards int    `json:"dashboards"`
}

func (v *UiRequestReceipt) UnmarshalJSON(data []byte) error {
	type plain UiRequestReceipt
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// UiRequestRefusal is why a request was not published. String (and Error)
// is Rust's Display.
type UiRequestRefusal interface {
	isUiRequestRefusal()
	String() string
	Error() string
}

// UiRequestRefusalInvalid: the request itself cannot be carried out.
type UiRequestRefusalInvalid struct {
	Reason string
}

// UiRequestRefusalNoDashboard: nobody is looking: no dashboard is attached
// to apply it.
type UiRequestRefusalNoDashboard struct{}

func (UiRequestRefusalInvalid) isUiRequestRefusal()     {}
func (UiRequestRefusalNoDashboard) isUiRequestRefusal() {}

func (r UiRequestRefusalInvalid) String() string { return r.Reason }
func (r UiRequestRefusalInvalid) Error() string  { return r.Reason }

func (UiRequestRefusalNoDashboard) String() string {
	return "no dashboard is open to show this; ask the person to start `mj`, then send it again"
}

func (r UiRequestRefusalNoDashboard) Error() string { return r.String() }

// UiRequestCursor is the newest request a dashboard has applied, or the
// newest present when it attached, which it must not replay.
type UiRequestCursor struct {
	// set is false before any request (Rust's None).
	set         bool
	incarnation string
	id          uint64
}

// UiRequestCursorAfter starts after every request already published.
func UiRequestCursorAfter(requests []UiRequest) UiRequestCursor {
	var cursor UiRequestCursor
	for _, request := range requests {
		if !cursor.set || request.ID >= cursor.id {
			cursor = UiRequestCursor{set: true, incarnation: request.Incarnation, id: request.ID}
		}
	}
	return cursor
}

// TakeNew is the requests not yet applied, in order, advancing past them.
//
// A request from another daemon incarnation is new whatever its id: the
// daemon that published it started counting from one again.
func (c *UiRequestCursor) TakeNew(requests []UiRequest) []*UiRequest {
	var fresh []*UiRequest
	for i := range requests {
		request := &requests[i]
		if !c.set || c.incarnation != request.Incarnation || request.ID > c.id {
			fresh = append(fresh, request)
		}
	}
	slices.SortStableFunc(fresh, func(a, b *UiRequest) int { return cmp.Compare(a.ID, b.ID) })
	if len(fresh) > 0 {
		last := fresh[len(fresh)-1]
		*c = UiRequestCursor{set: true, incarnation: last.Incarnation, id: last.ID}
	}
	return fresh
}
