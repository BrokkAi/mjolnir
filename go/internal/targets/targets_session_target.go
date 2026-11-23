// Port of mj-targets/src/targets/session_target.rs.
//
// From a session's recorded target to the locator its commands run against.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// BackendLocator is the execution-plan locator for a session's stored
// target.
//
// The mapping itself lives in package core; this only pairs the stored
// locator with the target template the session was created against.
func BackendLocator(locator core.TargetLocator, session *core.SessionRecord, config *core.Config) (core.TargetsTargetLocator, error) {
	return RecordedBackendLocator(locator, session.ID, session.TargetTemplateID, session.TargetRuntime, config)
}

func RecordedBackendLocator(
	locator core.TargetLocator,
	sessionID string,
	targetTemplateID string,
	runtime *core.TargetRuntimeSettings,
	config *core.Config,
) (core.TargetsTargetLocator, error) {
	panic("unported: mj-targets/src/targets/session_target.rs recorded_backend_locator")
}

// WorkspaceRoot is the directory the session's repositories are checked out
// under. containerWorkspace is the session record's recorded container
// workspace, which only container locators use.
func WorkspaceRoot(backend *core.TargetsTargetLocator, containerWorkspace *string) string {
	switch backend.Kind {
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		return backend.Workspace
	case core.TargetsTargetLocatorLocalBare:
		return backend.WorkerRoot
	default:
		return core.ContainerWorkspaceRoot(containerWorkspace)
	}
}
