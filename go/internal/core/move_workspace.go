// Port of mj-core/src/move_workspace.rs.

// Retained Move source records from earlier releases.
//
// Milestone 5 retired Move's untracked-file inventory, picker, exclusions, and
// rsync staging. RetainedMoveSource and WorkspacePath remain so
// `mj move-sources` can still list and clean up sources retained by earlier
// releases.

package core

import (
	"encoding/json"
	"fmt"
	"strings"
)

type WorkspacePath struct {
	Repository string `json:"repository"`
	Path       string `json:"path"`
}

func (p *WorkspacePath) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "repository", "path"); err != nil {
		return err
	}
	type wire WorkspacePath
	return unmarshalDenyUnknown(data, (*wire)(p))
}

// Compare is the Rust derived Ord: repository, then path by component.
func (p WorkspacePath) Compare(other WorkspacePath) int {
	if order := strings.Compare(p.Repository, other.Repository); order != 0 {
		return order
	}
	return comparePaths(p.Path, other.Path)
}

func FormatBytes(bytes uint64) string {
	switch {
	case bytes >= 1_000_000_000:
		return fmt.Sprintf("%.2f GB", float64(bytes)/1_000_000_000.0)
	case bytes >= 1_000_000:
		return fmt.Sprintf("%.1f MB", float64(bytes)/1_000_000.0)
	case bytes >= 1_000:
		return fmt.Sprintf("%.1f KB", float64(bytes)/1_000.0)
	}
	return fmt.Sprintf("%d B", bytes)
}

// RetainedMoveSource is a stopped source retained because the user excluded
// files from a legacy Move.
type RetainedMoveSource struct {
	OperationID string          `json:"operation_id"`
	SessionID   string          `json:"session_id"`
	Source      SessionRecord   `json:"source"`
	Exclusions  []WorkspacePath `json:"exclusions"`
	CreatedAt   string          `json:"created_at"`
}

// MarshalJSON writes no exclusions as `[]`, as serde writes an empty Vec.
func (r RetainedMoveSource) MarshalJSON() ([]byte, error) {
	type wire RetainedMoveSource
	if r.Exclusions == nil {
		r.Exclusions = []WorkspacePath{}
	}
	return marshalSerdeJSON(wire(r))
}

func (r *RetainedMoveSource) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "operation_id", "session_id", "source", "exclusions", "created_at"); err != nil {
		return err
	}
	type wire RetainedMoveSource
	return json.Unmarshal(data, (*wire)(r))
}
