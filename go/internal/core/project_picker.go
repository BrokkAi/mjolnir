// Port of mj-core/src/project_picker.rs.

// Shared request and response types for repository discovery.

package core

import (
	"encoding/json"
	"fmt"
)

// ProjectDiscoveryRequest is a sealed interface over
// ProjectDiscoveryRequestGithub and ProjectDiscoveryRequestDirectory,
// serialized internally tagged by "kind".
type ProjectDiscoveryRequest interface {
	isProjectDiscoveryRequest()
}

type ProjectDiscoveryRequestGithub struct {
	Query string `json:"query"`
}

type ProjectDiscoveryRequestDirectory struct {
	Path   string `json:"path"`
	Filter string `json:"filter"`
}

func (ProjectDiscoveryRequestGithub) isProjectDiscoveryRequest()    {}
func (ProjectDiscoveryRequestDirectory) isProjectDiscoveryRequest() {}

func (r ProjectDiscoveryRequestGithub) MarshalJSON() ([]byte, error) {
	type wire ProjectDiscoveryRequestGithub
	return json.Marshal(struct {
		Kind string `json:"kind"`
		wire
	}{"github", wire(r)})
}

func (r ProjectDiscoveryRequestDirectory) MarshalJSON() ([]byte, error) {
	type wire ProjectDiscoveryRequestDirectory
	return json.Marshal(struct {
		Kind string `json:"kind"`
		wire
	}{"directory", wire(r)})
}

// UnmarshalProjectDiscoveryRequestJSON decodes a ProjectDiscoveryRequest for
// other packages (mj-client's DaemonAction::DiscoverProjects carries one).
func UnmarshalProjectDiscoveryRequestJSON(data []byte) (ProjectDiscoveryRequest, error) {
	return unmarshalProjectDiscoveryRequest(data)
}

// unmarshalProjectDiscoveryRequest decodes the "kind"-tagged form.
func unmarshalProjectDiscoveryRequest(data []byte) (ProjectDiscoveryRequest, error) {
	var wire struct {
		Kind   *string `json:"kind"`
		Query  *string `json:"query"`
		Path   *string `json:"path"`
		Filter string  `json:"filter"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return nil, err
	}
	if wire.Kind == nil {
		return nil, fmt.Errorf("missing field `kind`")
	}
	switch *wire.Kind {
	case "github":
		if wire.Query == nil {
			return nil, fmt.Errorf("missing field `query`")
		}
		return ProjectDiscoveryRequestGithub{Query: *wire.Query}, nil
	case "directory":
		if wire.Path == nil {
			return nil, fmt.Errorf("missing field `path`")
		}
		return ProjectDiscoveryRequestDirectory{Path: *wire.Path, Filter: wire.Filter}, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected `github` or `directory`", *wire.Kind)
}

type ProjectDiscovery struct {
	Entries   []ProjectEntry `json:"entries"`
	Directory *string        `json:"directory"`
	Parent    *string        `json:"parent"`
	Truncated bool           `json:"truncated"`
}

// MarshalJSON writes no entries as `[]`, as serde writes an empty Vec.
func (d ProjectDiscovery) MarshalJSON() ([]byte, error) {
	type wire ProjectDiscovery
	if d.Entries == nil {
		d.Entries = []ProjectEntry{}
	}
	return marshalSerdeJSON(wire(d))
}

func (d *ProjectDiscovery) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "entries", "truncated"); err != nil {
		return err
	}
	type wire ProjectDiscovery
	return json.Unmarshal(data, (*wire)(d))
}

type ProjectEntry struct {
	Name        string           `json:"name"`
	Source      string           `json:"source"`
	Description string           `json:"description"`
	Kind        ProjectEntryKind `json:"kind"`
}

type ProjectEntryKind string

const (
	ProjectEntryKindRepository ProjectEntryKind = "repository"
	ProjectEntryKindDirectory  ProjectEntryKind = "directory"
)

func (k *ProjectEntryKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch kind := ProjectEntryKind(text); kind {
	case ProjectEntryKindRepository, ProjectEntryKindDirectory:
		*k = kind
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `repository` or `directory`", text)
}
