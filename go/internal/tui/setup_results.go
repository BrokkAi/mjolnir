// Port of mj-tui/src/setup/results.rs.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SetupLoginOutcome is how an interactive sign-in ended. A cancel is the
// user's choice, not a failure (sealed: the SetupLoginOutcome* variants).
type SetupLoginOutcome interface {
	isSetupLoginOutcome()
}

type SetupLoginOutcomeSigned struct{}

type SetupLoginOutcomeCancelled struct{}

type SetupLoginOutcomeFailed struct{ Value string }

func (SetupLoginOutcomeSigned) isSetupLoginOutcome()    {}
func (SetupLoginOutcomeCancelled) isSetupLoginOutcome() {}
func (SetupLoginOutcomeFailed) isSetupLoginOutcome()    {}

// SetupLoginOutcomeFromResult is Rust's From<Result<(), String>>: signed in,
// or failed with err's message.
func SetupLoginOutcomeFromResult(err error) SetupLoginOutcome {
	if err == nil {
		return SetupLoginOutcomeSigned{}
	}
	return SetupLoginOutcomeFailed{Value: err.Error()}
}

// BuildCachePreviewed applies a build cache resolution to every machine of
// the open Settings that still asks for key. A nil preview means the target
// kind cannot share a cache.
func (d *DashboardState) BuildCachePreviewed(generation uint64, key any, preview *core.BuildCachePreview, err error, installMbxAvailable bool) {
	panic("unported: mj-tui/src/setup/results.rs build_cache_previewed")
}

// MbxInstallStarted marks the confirmed host mutation busy before the CLI
// starts its worker.
func (d *DashboardState) MbxInstallStarted(generation uint64, key any, machineID string) bool {
	panic("unported: mj-tui/src/setup/results.rs mbx_install_started")
}

// MbxInstallFinished reports the install result (message or resultErr) and
// applies its fresh cache preview (preview or previewErr) when the settings
// draft still describes the machine that was installed.
func (d *DashboardState) MbxInstallFinished(generation uint64, key any, machineID string, message string, resultErr error, preview *core.BuildCachePreview, previewErr error, installMbxAvailable bool) {
	panic("unported: mj-tui/src/setup/results.rs mbx_install_finished")
}

// ArchiveSpacePreviewed takes the space an archive window would reclaim,
// dropping an answer the user has already typed past.
func (d *DashboardState) ArchiveSpacePreviewed(generation uint64, olderThanDays *uint32, preview core.ArchiveSpacePreview, err error) {
	panic("unported: mj-tui/src/setup/results.rs archive_space_previewed")
}

// SetupPathResolved applies a target path's expanded home directory
// (resolved, or err).
func (d *DashboardState) SetupPathResolved(generation uint64, draft any, path []string, value string, resolved string, err error) {
	panic("unported: mj-tui/src/setup/results.rs setup_path_resolved")
}

// SetupLoginFinished reports the result of an interactive sign-in started
// from the profiles page. A dialog that closed while the command ran reports
// on the dashboard instead.
func (d *DashboardState) SetupLoginFinished(generation uint64, profileID string, result SetupLoginOutcome) {
	panic("unported: mj-tui/src/setup/results.rs setup_login_finished")
}

// SetupSaved applies a Settings save: the saved configuration, or err.
func (d *DashboardState) SetupSaved(generation uint64, config core.Config, err error) {
	panic("unported: mj-tui/src/setup/results.rs setup_saved")
}

// SetupDiscovered merges a detection run into the open Settings draft.
func (d *DashboardState) SetupDiscovered(generation uint64, detection SetupDetection, err error) {
	panic("unported: mj-tui/src/setup/results.rs setup_discovered")
}

// machineNoteLines is how many lines a machine row's missing-runtime reason
// may take.
const machineNoteLines = 3

// machineWithoutRuntimeNote is why machine id in the Settings draft has no
// runtime, or false when a runtime in the draft runs on it. A machine as the
// configuration has it reports what its last check found; one the draft adds
// or changes reports what saving it will do.
func machineWithoutRuntimeNote(dashboard *DashboardState, dialog *SetupDialog, id string) (string, bool) {
	panic("unported: mj-tui/src/setup/results.rs machine_without_runtime_note")
}

// detectionNotice is what the Settings screen says after a detection run:
// the entries it added by name, and for runtimes how many it turned down,
// pointing at the Detection results view for each reason and fix. When it
// added nothing, it says whether it found nothing at all or found only what
// Settings already lists (launch finding R13-4).
func detectionNotice(scope DetectScope, foundAny bool, added []string, rejected []RejectedRuntime) string {
	panic("unported: mj-tui/src/setup/results.rs detection_notice")
}

// setupDialogMut is the open Settings dialog, or nil when mode is another
// one.
func setupDialogMut(mode Mode) *SetupDialog {
	panic("unported: mj-tui/src/setup/results.rs setup_dialog_mut")
}

// QueueSetupLogin holds a managed profile's login until the save result and
// updated configuration have been applied.
func (d *DashboardState) QueueSetupLogin(generation uint64, profileID string) {
	panic("unported: mj-tui/src/setup/results.rs queue_setup_login")
}

// TakeSetupLogin takes the queued managed-profile login.
func (d *DashboardState) TakeSetupLogin() (generation uint64, profileID string, ok bool) {
	panic("unported: mj-tui/src/setup/results.rs take_setup_login")
}
