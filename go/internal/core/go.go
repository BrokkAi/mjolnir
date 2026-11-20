// Port of mj-core/src/go.rs.

// Opt-in fast-start preferences, separate from the main configuration schema.

package core

import (
	"encoding/json"
	"fmt"
)

type GoRecipe struct {
	ProfileID             string  `json:"profile_id"`
	TargetID              string  `json:"target_id"`
	BundleID              *string `json:"bundle_id"`
	ProjectDirectory      *string `json:"project_directory"`
	CreateManagedWorktree *bool   `json:"create_managed_worktree"`
	// Also read as `mjolnir_subagents`; stored legacy booleans are accepted.
	// nil is Rust's None.
	Subagents        *SubagentPolicy   `json:"subagents"`
	AdditionalMounts []AdditionalMount `json:"additional_mounts"`
	// nil is Rust's None.
	ResourceAllocation SessionResourceAllocation `json:"resource_allocation"`
}

// UnmarshalJSON follows serde: `subagents` has the alias `mjolnir_subagents`
// and the custom DeserializeOptionalPolicy; the interface-typed fields
// decode through their sealed-interface helpers; fields without a default
// other than Option ones are required.
func (r *GoRecipe) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, required := range []string{"profile_id", "target_id", "additional_mounts"} {
		if _, ok := fields[required]; !ok {
			return fmt.Errorf("missing field `%s`", required)
		}
	}
	var wire struct {
		ProfileID             string            `json:"profile_id"`
		TargetID              string            `json:"target_id"`
		BundleID              *string           `json:"bundle_id"`
		ProjectDirectory      *string           `json:"project_directory"`
		CreateManagedWorktree *bool             `json:"create_managed_worktree"`
		AdditionalMounts      []AdditionalMount `json:"additional_mounts"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	recipe := GoRecipe{
		ProfileID:             wire.ProfileID,
		TargetID:              wire.TargetID,
		BundleID:              wire.BundleID,
		ProjectDirectory:      wire.ProjectDirectory,
		CreateManagedWorktree: wire.CreateManagedWorktree,
		AdditionalMounts:      wire.AdditionalMounts,
	}
	subagents, primary := fields["subagents"]
	alias, aliased := fields["mjolnir_subagents"]
	if primary && aliased {
		return fmt.Errorf("duplicate field `subagents`")
	}
	if aliased {
		subagents = alias
	}
	if primary || aliased {
		policy, err := DeserializeOptionalPolicy(subagents)
		if err != nil {
			return err
		}
		recipe.Subagents = policy
	}
	if raw, ok := fields["resource_allocation"]; ok && string(raw) != "null" {
		allocation, err := unmarshalSessionResourceAllocation(raw)
		if err != nil {
			return err
		}
		recipe.ResourceAllocation = allocation
	}
	*r = recipe
	return nil
}

// Defaults: project paths and repository choices never leak into another
// folder.
func (r *GoRecipe) Defaults() GoRecipe {
	defaults := *r
	defaults.BundleID = nil
	defaults.ProjectDirectory = nil
	defaults.AdditionalMounts = []AdditionalMount{}
	return defaults
}

type projectRecipe struct {
	Directory string   `json:"directory"`
	Recipe    GoRecipe `json:"recipe"`
}

type GoWorkspace struct {
	Directory     string  `json:"directory"`
	WorkspaceID   string  `json:"workspace_id"`
	LastSessionID *string `json:"last_session_id"`
}

// RecentLaunch is the latest interactive choices for the dashboard's next
// new-session form.
type RecentLaunch struct {
	ProfileID        string  `json:"profile_id"`
	TargetID         string  `json:"target_id"`
	BundleID         string  `json:"bundle_id"`
	ProjectDirectory *string `json:"project_directory,omitempty"`
}

type GoPreferences struct {
	version    uint32
	Default    *GoRecipe
	Recent     *RecentLaunch
	projects   []projectRecipe
	workspaces []GoWorkspace
}

// goPreferencesWire is the serde field order and attributes of GoPreferences.
type goPreferencesWire struct {
	Version    uint32          `json:"version"`
	Default    *GoRecipe       `json:"default"`
	Recent     *RecentLaunch   `json:"recent,omitempty"`
	Projects   []projectRecipe `json:"projects"`
	Workspaces []GoWorkspace   `json:"workspaces"`
}

func (p GoPreferences) MarshalJSON() ([]byte, error) {
	projects := p.projects
	if projects == nil {
		projects = []projectRecipe{}
	}
	workspaces := p.workspaces
	if workspaces == nil {
		workspaces = []GoWorkspace{}
	}
	return json.Marshal(goPreferencesWire{
		Version:    p.version,
		Default:    p.Default,
		Recent:     p.Recent,
		Projects:   projects,
		Workspaces: workspaces,
	})
}

func (p *GoPreferences) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, required := range []string{"version", "projects"} {
		if _, ok := fields[required]; !ok {
			return fmt.Errorf("missing field `%s`", required)
		}
	}
	var wire goPreferencesWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*p = GoPreferences{
		version:    wire.Version,
		Default:    wire.Default,
		Recent:     wire.Recent,
		projects:   wire.Projects,
		workspaces: wire.Workspaces,
	}
	return nil
}

// DefaultGoPreferences is the Rust Default impl.
func DefaultGoPreferences() GoPreferences {
	return GoPreferences{version: 2}
}

func GoPreferencesRememberRecent(path string, recent RecentLaunch) error {
	return goPreferencesUpdate(path, func(preferences *GoPreferences) {
		preferences.Recent = &recent
	})
}

func (p *GoPreferences) Workspaces() []GoWorkspace { return p.workspaces }

func (p *GoPreferences) Workspace(directory string) *GoWorkspace {
	panic("unported: mj-core/src/go.rs GoPreferences::workspace")
}

func GoPreferencesBindWorkspace(path string, directory string, workspaceID string) error {
	panic("unported: mj-core/src/go.rs GoPreferences::bind_workspace")
}

func GoPreferencesRememberSession(path string, directory string, workspaceID string, sessionID string) error {
	panic("unported: mj-core/src/go.rs GoPreferences::remember_session")
}

func goPreferencesUpdate(path string, edit func(*GoPreferences)) error {
	panic("unported: mj-core/src/go.rs GoPreferences::update")
}

func GoPreferencesDirectoryLabel(directory string) string {
	panic("unported: mj-core/src/go.rs GoPreferences::directory_label")
}

// GoPreferencesWorkspaceName locates workspaces created by the first
// implementation without renaming unrelated workspaces.
func GoPreferencesWorkspaceName(directory string) string {
	panic("unported: mj-core/src/go.rs GoPreferences::workspace_name")
}

func GoPreferencesPath() string {
	panic("unported: mj-core/src/go.rs GoPreferences::path")
}

func GoPreferencesLoad(path string) (GoPreferences, error) {
	panic("unported: mj-core/src/go.rs GoPreferences::load")
}

func (p *GoPreferences) Recipe(directory string) *GoRecipe {
	panic("unported: mj-core/src/go.rs GoPreferences::recipe")
}

// GoPreferencesRememberFirstPair records a profile and target an interactive
// session was created with as the default pair, when no default is saved
// yet.
//
// Only the first interactive creation sets it, the same rule `mj go` follows
// for a folder it was not told to make global. A chosen default is never
// replaced by whichever pair was used last. Without this only `mj go` saved
// a default, so `mj new` with no `--profile` failed after sessions made
// through another interactive surface.
func GoPreferencesRememberFirstPair(path string, profileID string, targetID string) error {
	panic("unported: mj-core/src/go.rs GoPreferences::remember_first_pair")
}

func GoPreferencesSaveRecipe(path string, directory string, recipe GoRecipe, global bool) error {
	panic("unported: mj-core/src/go.rs GoPreferences::save_recipe")
}
