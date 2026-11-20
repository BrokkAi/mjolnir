// Port of mj-core/src/go.rs.

// Opt-in fast-start preferences, separate from the main configuration schema.

package core

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"unicode"
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

// MarshalJSON writes no additional mounts as `[]`, as serde writes an empty
// Vec.
func (r GoRecipe) MarshalJSON() ([]byte, error) {
	type wire GoRecipe
	if r.AdditionalMounts == nil {
		r.AdditionalMounts = []AdditionalMount{}
	}
	return marshalSerdeJSON(wire(r))
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
	return marshalSerdeJSON(goPreferencesWire{
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
	for i := range p.workspaces {
		if comparePaths(p.workspaces[i].Directory, directory) == 0 {
			return &p.workspaces[i]
		}
	}
	return nil
}

func GoPreferencesBindWorkspace(path string, directory string, workspaceID string) error {
	return goPreferencesUpdate(path, func(preferences *GoPreferences) {
		if workspace := preferences.Workspace(directory); workspace != nil {
			if workspace.WorkspaceID != workspaceID {
				workspace.WorkspaceID = workspaceID
				workspace.LastSessionID = nil
			}
			return
		}
		preferences.workspaces = append(preferences.workspaces, GoWorkspace{
			Directory:   directory,
			WorkspaceID: workspaceID,
		})
	})
}

func GoPreferencesRememberSession(path string, directory string, workspaceID string, sessionID string) error {
	lock, err := configLockAcquire(path)
	if err != nil {
		return err
	}
	defer lock.Close()
	preferences, err := GoPreferencesLoad(path)
	if err != nil {
		return err
	}
	var workspace *GoWorkspace
	for i := range preferences.workspaces {
		entry := &preferences.workspaces[i]
		if comparePaths(entry.Directory, directory) == 0 && entry.WorkspaceID == workspaceID {
			workspace = entry
			break
		}
	}
	if workspace == nil {
		return errors.New("fast-start workspace binding changed; selection was not saved")
	}
	workspace.LastSessionID = &sessionID
	preferences.version = 2
	return goPreferencesWrite(path, &preferences)
}

func goPreferencesUpdate(path string, edit func(*GoPreferences)) error {
	lock, err := configLockAcquire(path)
	if err != nil {
		return err
	}
	defer lock.Close()
	preferences, err := GoPreferencesLoad(path)
	if err != nil {
		return err
	}
	edit(&preferences)
	preferences.version = 2
	return goPreferencesWrite(path, &preferences)
}

// goPreferencesWrite stores preferences as serde_json::to_vec_pretty does.
func goPreferencesWrite(path string, preferences *GoPreferences) error {
	body, err := marshalSerdeJSONPretty(preferences)
	if err != nil {
		return err
	}
	return AtomicWrite(path, body)
}

// goLabel keeps the first `limit` characters of a name that are not control
// characters.
func goLabel(name string, limit int) string {
	var label strings.Builder
	count := 0
	for _, c := range name {
		if count == limit {
			break
		}
		if unicode.IsControl(c) {
			continue
		}
		label.WriteRune(c)
		count++
	}
	return label.String()
}

func GoPreferencesDirectoryLabel(directory string) string {
	name, ok := pathFileName(directory)
	if !ok {
		name = directory
	}
	name = goLabel(strings.ToValidUTF8(name, "�"), 48)
	if strings.TrimSpace(name) == "" {
		return "Project"
	}
	return name
}

// GoPreferencesWorkspaceName locates workspaces created by the first
// implementation without renaming unrelated workspaces.
func GoPreferencesWorkspaceName(directory string) string {
	digest := sha256.Sum256([]byte(directory))
	identity := hex.EncodeToString(digest[:16])
	leaf, _ := pathFileName(directory)
	label := goLabel(strings.ToValidUTF8(leaf, "�"), 24)
	return fmt.Sprintf("go: %s %s", label, identity)
}

func GoPreferencesPath() string {
	return filepath.Join(filepath.Dir(ConfigPath()), "go.json")
}

func GoPreferencesLoad(path string) (GoPreferences, error) {
	body, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return DefaultGoPreferences(), nil
	}
	if err != nil {
		return GoPreferences{}, fmt.Errorf("read fast-start preferences: %w", err)
	}
	var preferences GoPreferences
	if err := json.Unmarshal(body, &preferences); err != nil {
		return GoPreferences{}, fmt.Errorf("read fast-start preferences: %w", err)
	}
	if preferences.version != 1 && preferences.version != 2 {
		return GoPreferences{}, fmt.Errorf("unsupported fast-start preferences version %d", preferences.version)
	}
	return preferences, nil
}

func (p *GoPreferences) Recipe(directory string) *GoRecipe {
	for i := range p.projects {
		if comparePaths(p.projects[i].Directory, directory) == 0 {
			recipe := p.projects[i].Recipe
			return &recipe
		}
	}
	if p.Default == nil {
		return nil
	}
	recipe := *p.Default
	return &recipe
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
	preferences, err := GoPreferencesLoad(path)
	if err != nil {
		return err
	}
	if preferences.Default != nil {
		return nil
	}
	return goPreferencesUpdate(path, func(preferences *GoPreferences) {
		if preferences.Default == nil {
			preferences.Default = &GoRecipe{
				ProfileID:        profileID,
				TargetID:         targetID,
				AdditionalMounts: []AdditionalMount{},
			}
		}
	})
}

func GoPreferencesSaveRecipe(path string, directory string, recipe GoRecipe, global bool) error {
	return goPreferencesUpdate(path, func(preferences *GoPreferences) {
		if global || preferences.Default == nil {
			defaults := recipe.Defaults()
			preferences.Default = &defaults
		}
		kept := preferences.projects[:0]
		for _, project := range preferences.projects {
			if comparePaths(project.Directory, directory) != 0 {
				kept = append(kept, project)
			}
		}
		preferences.projects = append(kept, projectRecipe{Directory: directory, Recipe: recipe})
	})
}
