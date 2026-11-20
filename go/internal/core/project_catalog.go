// Port of mj-core/src/project_catalog.rs.

// Shared project catalog views consumed by directory and project pickers.

package core

import (
	"encoding/json"
	"fmt"
)

type SavedProject struct {
	BundleID string                `json:"bundle_id"`
	Name     string                `json:"name"`
	Project  ProjectBundleSnapshot `json:"project"`
}

type ProjectLocation struct {
	Host string `json:"host"`
	// The original path is retained for lookup; suggestions use CheckoutRoot.
	Directory      string             `json:"directory"`
	CheckoutRoot   string             `json:"checkout_root"`
	RepositoryRoot string             `json:"repository_root"`
	Identity       RepositoryIdentity `json:"identity"`
	SeenAt         string             `json:"seen_at"`
}

func (l *ProjectLocation) UnmarshalJSON(data []byte) error {
	type wire ProjectLocation
	var location struct {
		wire
		Identity json.RawMessage `json:"identity"`
	}
	if err := json.Unmarshal(data, &location); err != nil {
		return err
	}
	if location.Identity == nil {
		return fmt.Errorf("missing field `identity`")
	}
	identity, err := unmarshalRepositoryIdentity(location.Identity)
	if err != nil {
		return err
	}
	*l = ProjectLocation(location.wire)
	l.Identity = identity
	return nil
}

// ProjectCatalogState is the "state" tag of ProjectCatalogStatus. The zero
// value is the Rust default, Ready.
type ProjectCatalogState int

const (
	ProjectCatalogStateReady ProjectCatalogState = iota
	ProjectCatalogStateRefreshing
	ProjectCatalogStateFailed
)

// ProjectCatalogStatus is the Rust enum Ready | Refreshing | Failed { errors }
// as one struct: Errors is meaningful only when State is Failed. It
// serializes internally tagged by "state".
type ProjectCatalogStatus struct {
	State  ProjectCatalogState
	Errors []string
}

func (s ProjectCatalogStatus) MarshalJSON() ([]byte, error) {
	switch s.State {
	case ProjectCatalogStateReady:
		return []byte(`{"state":"ready"}`), nil
	case ProjectCatalogStateRefreshing:
		return []byte(`{"state":"refreshing"}`), nil
	case ProjectCatalogStateFailed:
		errors := s.Errors
		if errors == nil {
			errors = []string{}
		}
		return json.Marshal(struct {
			State  string   `json:"state"`
			Errors []string `json:"errors"`
		}{"failed", errors})
	}
	return nil, fmt.Errorf("invalid ProjectCatalogState %d", int(s.State))
}

func (s *ProjectCatalogStatus) UnmarshalJSON(data []byte) error {
	var wire struct {
		State  *string   `json:"state"`
		Errors *[]string `json:"errors"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.State == nil {
		return fmt.Errorf("missing field `state`")
	}
	switch *wire.State {
	case "ready":
		*s = ProjectCatalogStatus{State: ProjectCatalogStateReady}
	case "refreshing":
		*s = ProjectCatalogStatus{State: ProjectCatalogStateRefreshing}
	case "failed":
		if wire.Errors == nil {
			return fmt.Errorf("missing field `errors`")
		}
		*s = ProjectCatalogStatus{State: ProjectCatalogStateFailed, Errors: *wire.Errors}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `ready`, `refreshing`, `failed`", *wire.State)
	}
	return nil
}

// FailureSummary: discovery details are logged by the daemon; keep picker
// notices bounded.
func (s *ProjectCatalogStatus) FailureSummary() *string {
	if s.State != ProjectCatalogStateFailed {
		return nil
	}
	count := len(s.Errors)
	noun := "errors"
	if count == 1 {
		noun = "error"
	}
	summary := fmt.Sprintf("Project discovery reported %d %s. See daemon logs for details.", count, noun)
	return &summary
}

type ProjectCatalogView struct {
	Projects  []SavedProject       `json:"projects"`
	Locations []ProjectLocation    `json:"locations"`
	Status    ProjectCatalogStatus `json:"status"`
}
