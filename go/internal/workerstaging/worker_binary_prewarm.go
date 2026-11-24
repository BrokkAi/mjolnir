// Port of mj-worker-staging/src/worker_binary/prewarm.rs.

// Sending a host the shareable content a later reviewer launch will need.

package workerstaging

import (
	"errors"
	"fmt"
	"log/slog"
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	locator := session.Target
	if locator == nil {
		return nil, errors.New("session target is missing")
	}
	backend, err := targets.BackendLocator(locator, session, config)
	if err != nil {
		return nil, err
	}
	ssh := SharedContentSSH(&backend)
	if ssh == nil {
		return PrewarmOutcomeNotApplicable{Reason: "the session's target does not use the shared profile store"}, nil
	}
	profile, ok := config.EnabledProfile(profileID)
	if !ok {
		return nil, fmt.Errorf("profile %s is unavailable", core.RustDebugString(profileID))
	}
	staging, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return nil, fmt.Errorf("create profile prewarm staging: %w", rustIOError(err))
	}
	defer os.RemoveAll(staging)
	stage := pathJoin(staging, "profile")
	if err := StageProfile(&profile, stage); err != nil {
		return nil, err
	}
	// The provider's model catalog (`models.json`) is a shared file, but
	// fetching it is a network wait a cancelled daemon cannot interrupt.
	// Prewarm offers the catalog only when this daemon already holds a copy
	// from an earlier launch; without one it is left for the launch that
	// needs it.
	refuseFetch := func(string, string) ([]byte, error) {
		return nil, errors.New("prewarm does not fetch model catalogs")
	}
	if err := StageCodexCatalog(profileID, &profile, stage, refuseFetch, SharedCatalogCache{}); err != nil {
		slog.Debug("prewarm skips the model catalog", "profile_id", profileID, "error", err.Error())
		// A failed catalog step may have left a partial change in the stage;
		// stage the plain profile again rather than offer it.
		os.RemoveAll(stage)
		if err := StageProfile(&profile, stage); err != nil {
			return nil, err
		}
	}
	if err := StageManagedSkills(profile.Kind, stage, locator.SkillsScope()); err != nil {
		return nil, err
	}
	roots := SharedProfilePaths(profile.Kind)
	manifest, err := ProfileContentManifestFromStaged(stage, roots)
	if err != nil {
		return nil, err
	}
	transferred, err := manifest.TryTransfer(executor, ssh, nil)
	if err != nil {
		return nil, err
	}
	if transferred {
		return PrewarmOutcomeWarm{}, nil
	}
	return PrewarmOutcomeNotApplicable{Reason: "the host lacks the profile receiver runtime"}, nil
}
