// Port of mj-worker-staging/src/worker_binary/prewarm.rs.

// Sending a host the shareable content a later reviewer launch will need.

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PrewarmOutcome is what one prewarm did for one host and profile:
// PrewarmOutcomeWarm or PrewarmOutcomeNotApplicable. Rust enum; the
// variants are comparable with ==.
type PrewarmOutcome interface {
	isPrewarmOutcome()
}

// PrewarmOutcomeWarm: the host now holds this profile's shareable objects
// (the negotiation sent only the ones it lacked).
type PrewarmOutcomeWarm struct{}

// PrewarmOutcomeNotApplicable: nothing to send. Reason says why the
// session's host takes no shared profile content.
type PrewarmOutcomeNotApplicable struct {
	Reason string
}

func (PrewarmOutcomeWarm) isPrewarmOutcome()          {}
func (PrewarmOutcomeNotApplicable) isPrewarmOutcome() {}

// SharedContentSSH is the SSH account that holds this locator's shared
// profile content, when its launches use the shared store; nil otherwise. A
// Nitro guest does not share its profile objects with the hypervisor account
// or another guest, and a local target has no link to spare.
func SharedContentSSH(locator *core.TargetsTargetLocator) *core.SshTarget {
	switch locator.Kind {
	case core.TargetsTargetLocatorAwsEc2, core.TargetsTargetLocatorSshBare, core.TargetsTargetLocatorSshPodman,
		core.TargetsTargetLocatorSshDocker, core.TargetsTargetLocatorSshAppleContainer:
		return locator.SSH
	default:
		return nil
	}
}

// PrewarmProfileContent stages profileID's shareable files the way a
// reviewer launch stages them and offers the host the shareable objects, so
// the launch that follows sends only its private bundle. It rides the
// negotiation a launch uses, on the bulk lane, with no operation deadline:
// the executor's cancellation is what stops it.
func PrewarmProfileContent(config *core.Config, session *core.SessionRecord, profileID string, executor core.CommandExecutor) (PrewarmOutcome, error) {
	panic("unported: mj-worker-staging/src/worker_binary/prewarm.rs prewarm_profile_content")
}
