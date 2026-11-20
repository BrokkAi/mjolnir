// Port of mj-core/src/workspace.rs.
//
// Durable user-facing workspace identities and name validation.

package core

import (
	"encoding/json"
	"fmt"
	"math"
	"strconv"
)

// DefaultWorkspaceID is the stable identity assigned to the workspace that
// receives pre-workspace data.
const DefaultWorkspaceID = "default"

// PaneSize is the explicit height requested for one support pane in the
// dashboard.
//
// Its serialized spelling is shared by the global sidebar preference and
// the persisted workspace support-pane layout. The Rust default is Standard.
type PaneSize string

const (
	PaneSizeMinimized PaneSize = "minimized"
	PaneSizeStandard  PaneSize = "standard"
	PaneSizeMaximized PaneSize = "maximized"
)

func (s PaneSize) IsDefault() bool { return s == PaneSizeStandard }

// Cycled is the next title-bar control, wrapping from maximum to minimum.
func (s PaneSize) Cycled() PaneSize {
	switch s {
	case PaneSizeMinimized:
		return PaneSizeStandard
	case PaneSizeStandard:
		return PaneSizeMaximized
	}
	return PaneSizeMinimized
}

// PaneSizes is the persisted sizes for the dashboard's three support panes.
type PaneSizes struct {
	Sessions PaneSize `json:"sessions"`
	Targets  PaneSize `json:"targets"`
	Quota    PaneSize `json:"quota"`
}

// NewPaneSizes is PaneSizes::default.
func NewPaneSizes() PaneSizes {
	return PaneSizes{PaneSizeStandard, PaneSizeStandard, PaneSizeStandard}
}

// Validate validates the exclusive maximum before applying a saved arrangement.
func (s PaneSizes) Validate() error {
	maximized := 0
	for _, size := range []PaneSize{s.Sessions, s.Targets, s.Quota} {
		if size == PaneSizeMaximized {
			maximized++
		}
	}
	if maximized > 1 {
		return fmt.Errorf("at most one pane can be maximized")
	}
	return nil
}

func (s PaneSizes) AllStandard() bool {
	return s.Sessions == PaneSizeStandard && s.Targets == PaneSizeStandard && s.Quota == PaneSizeStandard
}

// SplitAxis is which way a split divides its pane.
//
// Horizontal means the two children sit side by side, so the divider drawn
// between them is vertical. This matches ratatui's Direction::Horizontal.
type SplitAxis string

const (
	SplitAxisHorizontal SplitAxis = "horizontal"
	SplitAxisVertical   SplitAxis = "vertical"
)

// LayoutNode is one node of the conversation area's binary space
// partition: either a pane or a split holding two children. Tag "kind",
// snake_case.
type LayoutNode interface {
	isLayoutNode()
	// collectPaneIDs collects every pane id in tree order, reporting the
	// first duplicate.
	collectPaneIDs(found *[]uint32) error
}

type LayoutNodePane struct {
	ID uint32 `json:"id"`
}

type LayoutNodeSplit struct {
	Axis SplitAxis `json:"axis"`
	// Ratio is the first child's share of the split, between 0.1 and 0.9.
	Ratio  float32    `json:"ratio"`
	First  LayoutNode `json:"first"`
	Second LayoutNode `json:"second"`
}

func (LayoutNodePane) isLayoutNode()  {}
func (LayoutNodeSplit) isLayoutNode() {}

func (n LayoutNodePane) collectPaneIDs(found *[]uint32) error {
	for _, id := range *found {
		if id == n.ID {
			return fmt.Errorf("pane id %d appears more than once in the layout", n.ID)
		}
	}
	*found = append(*found, n.ID)
	return nil
}

func (n LayoutNodeSplit) collectPaneIDs(found *[]uint32) error {
	ratio := strconv.FormatFloat(float64(n.Ratio), 'g', -1, 32)
	if math.IsNaN(float64(n.Ratio)) || math.IsInf(float64(n.Ratio), 0) {
		return fmt.Errorf("split ratio %s is not a finite number", ratio)
	}
	if n.Ratio < 0.1 || n.Ratio > 0.9 {
		return fmt.Errorf("split ratio %s is outside 0.1..=0.9", ratio)
	}
	if err := n.First.collectPaneIDs(found); err != nil {
		return err
	}
	return n.Second.collectPaneIDs(found)
}

func (n LayoutNodePane) MarshalJSON() ([]byte, error) {
	type plain LayoutNodePane
	return stateMarshalTagged("kind", "pane", plain(n))
}

func (n LayoutNodeSplit) MarshalJSON() ([]byte, error) {
	type plain LayoutNodeSplit
	return stateMarshalTagged("kind", "split", plain(n))
}

func (n *LayoutNodeSplit) UnmarshalJSON(data []byte) error {
	type plain LayoutNodeSplit
	aux := struct {
		*plain
		First  stateUnionJSON[LayoutNode] `json:"first"`
		Second stateUnionJSON[LayoutNode] `json:"second"`
	}{plain: (*plain)(n)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.First.V == nil || aux.Second.V == nil {
		return fmt.Errorf("split is missing a child")
	}
	n.First, n.Second = aux.First.V, aux.Second.V
	return nil
}

func unmarshalLayoutNode(data []byte) (LayoutNode, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "pane":
		return stateVariant[LayoutNodePane, LayoutNode](data)
	case "split":
		var node LayoutNodeSplit
		err := json.Unmarshal(data, &node)
		return node, err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// ConversationLayout is the persisted arrangement of the dashboard's
// conversation area for one workspace: the pane tree, the focused pane, and
// the session each pane shows. A pane with no entry in Sessions is empty.
type ConversationLayout struct {
	Root     LayoutNode        `json:"root"`
	Focus    uint32            `json:"focus"`
	Sessions map[uint32]string `json:"sessions"`
	// Browse is missing only in layouts saved before Browse/pins existed.
	Browse *uint32           `json:"browse"`
	Pins   map[string]uint32 `json:"pins"`
}

func (l ConversationLayout) MarshalJSON() ([]byte, error) {
	type plain ConversationLayout
	if l.Sessions == nil {
		l.Sessions = map[uint32]string{}
	}
	if l.Pins == nil {
		l.Pins = map[string]uint32{}
	}
	return json.Marshal(plain(l))
}

func (l *ConversationLayout) UnmarshalJSON(data []byte) error {
	type plain ConversationLayout
	aux := struct {
		*plain
		Root stateUnionJSON[LayoutNode] `json:"root"`
	}{plain: (*plain)(l)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.Root.V == nil {
		return fmt.Errorf("missing field `root`")
	}
	l.Root = aux.Root.V
	if l.Pins == nil {
		l.Pins = map[string]uint32{}
	}
	return nil
}

// NewConversationLayout is ConversationLayout::default.
func NewConversationLayout() ConversationLayout {
	browse := uint32(1)
	return ConversationLayout{
		Root:     LayoutNodePane{ID: 1},
		Focus:    1,
		Sessions: map[uint32]string{},
		Browse:   &browse,
		Pins:     map[string]uint32{},
	}
}

// Validate validates a layout before applying or storing it.
func (l *ConversationLayout) Validate() error {
	panic("unported: mj-core/src/workspace.rs ConversationLayout::validate")
}

type WorkspaceRecord struct {
	ID           string `json:"id"`
	Name         string `json:"name"`
	CreatedAt    string `json:"created_at"`
	LastOpenedAt string `json:"last_opened_at"`
	// SessionCount is the number of active sessions currently owned by this
	// workspace.
	SessionCount uint64 `json:"session_count"`
}

func (r *WorkspaceRecord) UnmarshalJSON(data []byte) error {
	type plain WorkspaceRecord
	return stateDecodeStrict(data, (*plain)(r))
}

type DetachedDraft struct {
	ID          string  `json:"id"`
	WorkspaceID string  `json:"workspace_id"`
	SessionID   *string `json:"session_id"`
	Source      string  `json:"source"`
	OwnerPid    *uint32 `json:"owner_pid"`
	SavedAt     string  `json:"saved_at"`
	Text        string  `json:"text"`
	RecoveredAt *string `json:"recovered_at"`
}

func (d *DetachedDraft) UnmarshalJSON(data []byte) error {
	type plain DetachedDraft
	return stateDecodeStrict(data, (*plain)(d))
}

// NormalizeWorkspaceName normalizes a display name and returns the
// case-insensitive uniqueness key.
func NormalizeWorkspaceName(name string) (string, string, error) {
	panic("unported: mj-core/src/workspace.rs normalize_workspace_name")
}

func NewWorkspaceID() (string, error) {
	panic("unported: mj-core/src/workspace.rs new_workspace_id")
}
