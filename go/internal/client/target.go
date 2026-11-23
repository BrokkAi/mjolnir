// Port of mj-client/src/target.rs.

// Pure target and resume compatibility contracts shared by control surfaces.

package client

import (
	"errors"
	"fmt"
	"runtime"
	"slices"
	"strings"

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
	target, ok := config.Targets[targetID]
	if !ok {
		return 0, fmt.Errorf("target %s is no longer configured", targetID)
	}
	projectDirectory, hasDirectory := checkout.ProjectDirectory()
	if !hasDirectory {
		if _, localBare := target.(core.TargetTemplateLocalBare); localBare {
			return workspaceToRawCompatibility(session, config)
		}
		return ResumePlanInPlace, nil
	}
	directory := projectDirectory
	// Only a worktree this session owns itself; a borrowed checkout is its
	// owner's.
	var worktree *core.ManagedWorktree
	if managed, ok := checkout.(core.CheckoutManagedWorktree); ok {
		worktree = managed.Worktree
	}
	if worktree == nil {
		previous, ok := config.Targets[session.TargetTemplateID]
		if !ok {
			return 0, errors.New("the bare target this session last used is no longer configured")
		}
		_, previousLocal := previous.(core.TargetTemplateLocalBare)
		_, targetLocal := target.(core.TargetTemplateLocalBare)
		if core.IsBareProjectTarget(target) {
			if previousLocal == targetLocal {
				return ResumePlanInPlace, nil
			}
			return 0, fmt.Errorf("this session opens %s directly on its host; resume it on the same kind of bare target", directory)
		}
		if session.Project != nil && session.Project.PrimaryIsPlainDirectory() {
			return 0, fmt.Errorf("%s is a directory project without a committed Git checkout; resume it on a local bare target", directory)
		}
		// A checkout on this machine can become an isolated workspace: the
		// resume re-snapshots it against its own network remote. Whether the
		// recorded directory really is a whole checkout with such a remote
		// needs Git, so the conversion plan decides that, not the picker.
		if previousLocal {
			return ResumePlanRawToWorkspace, nil
		}
		return 0, fmt.Errorf("this session opens %s on an SSH host; resume it on a bare target there", directory)
	}
	resumeTarget, err := ManagedWorktreeTarget(target)
	if err == nil {
		if resumeTarget.SameLocation(worktree.Target) {
			return ResumePlanInPlace, nil
		}
		return 0, fmt.Errorf("this session's working tree lives on %s; resume it there", managedWorktreeLocation(worktree.Target))
	}
	if _, local := worktree.Target.(core.ManagedWorktreeTargetLocal); !local {
		return 0, fmt.Errorf("this session works directly in %s on %s; resume it on a bare target there",
			directory, managedWorktreeLocation(worktree.Target))
	}
	// A whole managed worktree on this machine converts: the resume
	// re-snapshots it against the owning checkout's network remote.
	if targetPathEqual(worktree.WorktreeRoot, projectDirectory) {
		return ResumePlanRawToWorkspace, nil
	}
	return 0, fmt.Errorf("this session opens %s, a subdirectory of its checkout; resume it on a bare target", directory)
}

// targetPathEqual is PathBuf equality: the same components, so repeated
// separators, a trailing separator and inner `.` components do not count.
func targetPathEqual(a, b string) bool {
	components := func(path string) []string {
		separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
		var parts []string
		if path != "" && separator(rune(path[0])) {
			parts = append(parts, "/")
		}
		for index, part := range strings.FieldsFunc(path, separator) {
			if part == "." && !(index == 0 && parts == nil && strings.HasPrefix(path, ".")) {
				continue
			}
			parts = append(parts, part)
		}
		return parts
	}
	return slices.Equal(components(a), components(b))
}

// bundleOnLocalBare is why a bundle session cannot resume on a local bare
// target. A bare target has no managed workspace to restore the bundle into.
const bundleOnLocalBare = "this session was created from a project bundle; a local bare target only hosts raw project sessions — resume it on a container, SSH, or EC2 target"

// workspaceToRawCompatibility reports whether a bundle session can leave its
// workspace for a checkout on this machine. Only a single repository already
// on this machine can become one.
func workspaceToRawCompatibility(session *core.SessionRecord, config *core.Config) (ResumePlan, error) {
	// A host checkout can share or clone later history. Keep cutoff sessions
	// in their independent bundle workspace, just as they started at launch.
	if session.Checkout != nil && session.Checkout.NoLaterHistory {
		return ResumePlanInPlace, nil
	}
	bundle := session.ProjectBundle(config)
	if bundle == nil {
		return 0, errors.New(bundleOnLocalBare)
	}
	if len(bundle.Repositories) != 1 {
		return 0, fmt.Errorf("this session's project has %d repositories; a local bare target holds one checkout — resume it on a container, SSH, or EC2 target",
			len(bundle.Repositories))
	}
	if bundle.Repositories[0].Local == nil {
		return 0, errors.New("this session's project came from GitHub; resume it on a container, SSH, or EC2 target")
	}
	return ResumePlanWorkspaceToRaw, nil
}

// managedWorktreeLocation is where a managed worktree's checkout physically
// lives, in words a user reads.
func managedWorktreeLocation(target core.ManagedWorktreeTarget) string {
	if ssh, ok := target.(core.ManagedWorktreeTargetSsh); ok {
		return ssh.Destination
	}
	return "this machine"
}
