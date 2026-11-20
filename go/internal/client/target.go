// Port of mj-client/src/target.rs.

// Pure target and resume compatibility contracts shared by control surfaces.

package client

import (
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DefaultImage is the session image used when nothing names another. Session
// tools and harnesses are installed after cloning the project.
const DefaultImage = core.DefaultSessionImage

// ManagedWorktreeTarget converts a configured bare target to the durable
// target identity stored on managed worktrees.
func ManagedWorktreeTarget(template core.TargetTemplate) (core.ManagedWorktreeTarget, error) {
	switch template := template.(type) {
	case core.TargetTemplateLocalBare:
		return core.ManagedWorktreeTargetLocal{}, nil
	case core.TargetTemplateSshBare:
		ssh := template.SSH
		destination := ssh.Host
		if ssh.User != nil {
			destination = *ssh.User + "@" + ssh.Host
		}
		// The same arguments the controller's SSH backend uses. Only the
		// location they name (see core.ManagedWorktreeTarget.SameLocation)
		// decides whether a resume stays put.
		sshArgs := core.SSHArgsWithIdentity(ssh.ExtraArgs, ssh.IdentityFile)
		return core.ManagedWorktreeTargetSsh{Destination: destination, SSHArgs: sshArgs}, nil
	}
	return nil, errors.New("managed raw worktrees require a bare target")
}

// ResumePlan is what a resume has to do to the session record before it
// provisions.
type ResumePlan int

const (
	// ResumePlanInPlace keeps the session in the representation it already
	// has.
	ResumePlanInPlace ResumePlan = iota
	// ResumePlanRawToWorkspace moves a raw checkout session into a workspace
	// target as a bundle session. Only a whole checkout on this machine
	// reaches this plan; the conversion still requires that checkout to have
	// a network Git remote, which only Git can answer.
	ResumePlanRawToWorkspace
	// ResumePlanWorkspaceToRaw moves a bundle session out of its workspace
	// into a raw local worktree.
	ResumePlanWorkspaceToRaw
)

// ResumeCompatibility reports whether session may resume on targetID, and
// what the resume must do to the session record. The error is shown to the
// person choosing the target, so it says where the session is tied down and
// what to pick instead.
//
// This decides representation only. It performs no I/O, so it can run on
// every row of a target picker.
func ResumeCompatibility(session *core.SessionRecord, config *core.Config, targetID string) (ResumePlan, error) {
	checkout := session.RecordCheckout()
	return ResumeCompatibilityWithCheckout(session, checkout, config, targetID)
}

// ResumeCompatibilityWithCheckout is the same compatibility decision using a
// checkout resolved by State when the caller has the session's sub-agent
// relationship available.
func ResumeCompatibilityWithCheckout(
	session *core.SessionRecord,
	checkout core.Checkout,
	config *core.Config,
	targetID string,
) (ResumePlan, error) {
	panic("unported: mj-client/src/target.rs resume_compatibility_with_checkout")
}

// bundleOnLocalBare is why a bundle session cannot resume on a local bare
// target. A bare target has no managed workspace to restore the bundle into.
const bundleOnLocalBare = "this session was created from a project bundle; a local bare target only hosts raw project sessions — resume it on a container, SSH, or EC2 target"

// workspaceToRawCompatibility reports whether a bundle session can leave its
// workspace for a checkout on this machine. Only a single repository already
// on this machine can become one.
func workspaceToRawCompatibility(session *core.SessionRecord, config *core.Config) (ResumePlan, error) {
	panic("unported: mj-client/src/target.rs workspace_to_raw_compatibility")
}

// managedWorktreeLocation is where a managed worktree's checkout physically
// lives, in words a user reads.
func managedWorktreeLocation(target core.ManagedWorktreeTarget) string {
	if ssh, ok := target.(core.ManagedWorktreeTargetSsh); ok {
		return ssh.Destination
	}
	return "this machine"
}
