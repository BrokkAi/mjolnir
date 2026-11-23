// Port of mj-tui/src/go.rs.

// The optional fast-start workflow. All IO remains in the CLI supervisor.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const simpleTargetID = "localhost"

// GoMode is the fast-start workflow's state for one workspace.
type GoMode struct {
	WorkspaceID   *string
	LastSessionID *string
	Directory     string
	Recipe        *core.GoRecipe
	SaveAsDefault bool
	// Simple: `mj simple` fixes the target, checkout, review, and key
	// prefix. The startup state machine uses this marker rather than
	// inferring those invariants from a recipe that a normal `mj go` user
	// may edit.
	Simple bool
}

// RememberedGoLaunch is what RememberGoLaunch hands the host to save: the
// directory the recipe belongs to, the recipe, and whether it becomes the
// default.
type RememberedGoLaunch struct {
	Directory     string
	Recipe        core.GoRecipe
	SaveAsDefault bool
}

// SetLaunchProjectDirectory records the repository mj was started in, so a
// new local session starts with it as its project directory.
func (d *DashboardState) SetLaunchProjectDirectory(directory *string) {
	d.launchProjectDirectory = directory
}

func (d *DashboardState) RegisterGoWorkspaces(modes []GoMode) {
	for _, mode := range modes {
		if mode.WorkspaceID == nil {
			continue
		}
		if d.goWorkspaces == nil {
			d.goWorkspaces = map[string]GoMode{}
		}
		d.goWorkspaces[*mode.WorkspaceID] = mode
	}
}

func (d *DashboardState) switchGoWorkspace(workspaceID *string) {
	if mode := d.goMode; mode != nil && mode.WorkspaceID != nil {
		mode.LastSessionID = nil
		if selected, ok := d.SelectedSessionID(); ok {
			mode.LastSessionID = &selected
		}
		if d.goWorkspaces == nil {
			d.goWorkspaces = map[string]GoMode{}
		}
		d.goWorkspaces[*mode.WorkspaceID] = *mode
	}
	d.goMode = nil
	if workspaceID != nil {
		if mode, ok := d.goWorkspaces[*workspaceID]; ok {
			d.goMode = &mode
		}
	}
}

// SetGoContext records where a session's checkout is and on which branch
// (First and Second), or why that could not be read.
func (d *DashboardState) SetGoContext(sessionID string, result client.DaemonResult[client.DaemonPair[string, string]]) {
	if d.goContexts == nil {
		d.goContexts = map[string]client.DaemonResult[client.DaemonPair[string, string]]{}
	}
	d.goContexts[sessionID] = result
}

// BeginGo starts the fast-start workflow. startup is the session the daemon
// chose for this workspace (DaemonClient.GoStartupSession): the remembered
// one while it is still eligible, otherwise the newest.
func (d *DashboardState) BeginGo(mode GoMode, setup bool, startup *core.SessionRecord) DashboardAction {
	panic("unported: mj-tui/src/go.rs DashboardState::begin_go")
}

func (d *DashboardState) GoConversationTitle(sessionID string) string {
	panic("unported: mj-tui/src/go.rs DashboardState::go_conversation_title")
}

func (d *DashboardState) GoMode() *GoMode {
	return d.goMode
}

func (d *DashboardState) SimpleMode() bool {
	return d.simpleMode
}

func (d *DashboardState) changeGoSetup() DashboardAction {
	panic("unported: mj-tui/src/go.rs DashboardState::change_go_setup")
}

func (d *DashboardState) GoLaunchAction(workspaceID string, recipe core.GoRecipe) DashboardAction {
	panic("unported: mj-tui/src/go.rs DashboardState::go_launch_action")
}

// RememberGoLaunch records a launch's settings as its workspace's fast-start
// recipe and returns what the host should save, or nil when the launch is
// not one to remember.
func (d *DashboardState) RememberGoLaunch(action DashboardAction) *RememberedGoLaunch {
	panic("unported: mj-tui/src/go.rs DashboardState::remember_go_launch")
}

func (d *DashboardState) goContext() []string {
	panic("unported: mj-tui/src/go.rs DashboardState::go_context")
}
