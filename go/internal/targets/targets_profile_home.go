// Port of mj-targets/src/targets/profile_home.rs.
//
// Where a session's harness profile lives on its target.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TargetProfileHome(locator *core.TargetsTargetLocator, sessionID string, profile *core.HarnessProfile) string {
	root := RemovableProfileRoot(locator, sessionID, profile)
	if profile.Kind.NestedHome() {
		return core.Text(core.Join(root, profile.Kind.ID()))
	}
	return root
}

// RemovableProfileRoot is the per-session profile directory a session's
// harness home lives in, which the session owns and an in-place harness
// replacement or teardown may delete.
//
// This is the root that TargetProfileHome derives its answer from, not that
// answer itself: a harness with an XDG-nested home keeps its home in a
// subdirectory of a per-session root under the data directory, and the whole
// root is what belongs to the session.
func RemovableProfileRoot(locator *core.TargetsTargetLocator, sessionID string, profile *core.HarnessProfile) string {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		if profile.Kind.NestedHome() {
			return LocalNestedProfileRoot(sessionID)
		}
		return core.Text(core.Join(locator.WorkerRoot, "profile"))
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare:
		return ".local/share/hel/profiles/" + sessionID
	default:
		return "/var/lib/hel/profiles/" + sessionID
	}
}
