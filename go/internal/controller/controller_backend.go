// Port of mj-controller/src/controller/backend.rs (only what the dashboard
// calls so far; the bodies need the controller's backend locators and
// worker launch configuration, which arrive with the Go daemon).

package controller

import "github.com/BrokkAi/mjolnir/go/internal/core"

// SessionWorkingContext inspects the actual execution checkout in a
// background worker: the checkout directory and its branch.
func (c *Controller) SessionWorkingContext(sessionID string, executor core.CommandExecutor) (string, string, error) {
	panic("unported: mj-controller/src/controller/backend.rs Controller::session_working_context")
}

// SessionGitStatus is the session checkout's branch, distance from
// upstream, and changed files, read with the target's own `git`. Builds on
// SessionWorkingContext, so it works wherever that does: a local checkout, a
// container, or an SSH host.
func (c *Controller) SessionGitStatus(sessionID string, executor core.CommandExecutor) (core.SessionGitStatus, error) {
	panic("unported: mj-controller/src/controller/backend.rs Controller::session_git_status")
}
