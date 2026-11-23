// Port of mj-tui/src/setup/results.rs.

package tui

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.generation != generation {
		return
	}
	var machines []string
	for _, machineID := range slices.Sorted(maps.Keys(dialog.buildCachePreviews)) {
		state := dialog.buildCachePreviews[machineID]
		if setupValueEqual(state.key, key) && dialog.buildCacheKeyIs(machineID, key) {
			machines = append(machines, machineID)
		}
	}
	if len(machines) == 0 {
		return
	}
	var notice string
	switch {
	case err != nil:
		notice = fmt.Sprintf("Could not resolve the build cache defaults: %s", err.Error())
	case preview == nil:
		notice = "This target kind cannot share a build cache."
	default:
		hostMbx := "no mbx on the host"
		if preview.NativeMbx != nil {
			hostMbx = "host mbx " + *preview.NativeMbx
		}
		switch reason := preview.OffReason.(type) {
		case nil:
			notice = fmt.Sprintf("Sessions here share the build cache (%s).", hostMbx)
		// The reason already names the host's own mbx and what uses it.
		case core.BuildCacheOffUnsupportedPlatform:
			notice = reason.Value
		default:
			notice = fmt.Sprintf("The mbx build cache is unavailable: %s (%s).", reason.String(), hostMbx)
		}
	}
	var result BuildCachePreviewResult = BuildCachePreviewResultReady{value: preview}
	if err != nil {
		result = BuildCachePreviewResultFailed{value: err.Error()}
	}
	for _, machineID := range machines {
		dialog.buildCachePreviews[machineID] = BuildCachePreviewState{
			key:                 setupValueClone(key),
			result:              result,
			installMbxAvailable: installMbxAvailable,
		}
	}
	dialog.notice = &notice
	dialog.prepare()
}

// MbxInstallStarted marks the confirmed host mutation busy before the CLI
// starts its worker.
func (d *DashboardState) MbxInstallStarted(generation uint64, key any, machineID string) bool {
	dialog := setupDialogMut(d.mode)
	if dialog == nil {
		return false
	}
	if dialog.generation != generation || dialog.mbxInstall != nil {
		return false
	}
	offer, ok := dialog.installMbxOffer()
	if !ok {
		return false
	}
	if offer.machineID != machineID || !setupValueEqual(offer.key, key) {
		return false
	}
	host := machineHostLabel(machineID, offer.machine)
	dialog.mbxInstall = &MbxInstallState{
		key:       setupValueClone(key),
		machineID: machineID,
		upgrading: offer.upgrading,
	}
	notice := fmt.Sprintf("Installing mbx on %s…", host)
	if offer.upgrading {
		notice = fmt.Sprintf("Upgrading mbx on %s…", host)
	}
	dialog.notice = &notice
	dialog.prepare()
	return true
}

// MbxInstallFinished reports the install result (message or resultErr) and
// applies its fresh cache preview (preview or previewErr) when the settings
// draft still describes the machine that was installed.
func (d *DashboardState) MbxInstallFinished(generation uint64, key any, machineID string, message string, resultErr error, preview *core.BuildCachePreview, previewErr error, installMbxAvailable bool) {
	dialog := setupDialogMut(d.mode)
	if dialog == nil {
		d.SetNotice(formatMbxInstallResult(message, resultErr, nil))
		return
	}
	if dialog.generation != generation {
		notice := formatMbxInstallResult(message, resultErr, nil)
		dialog.notice = &notice
		dialog.prepare()
		return
	}
	var operation *MbxInstallState
	if install := dialog.mbxInstall; install != nil && install.machineID == machineID && setupValueEqual(install.key, key) {
		held := *install
		operation = &held
	}
	if operation != nil {
		dialog.mbxInstall = nil
	}
	notice := formatMbxInstallResult(message, resultErr, operation)
	if dialog.buildCacheKeyIs(machineID, key) {
		if previewErr == nil {
			dialog.buildCachePreviews[machineID] = BuildCachePreviewState{
				key:                 setupValueClone(key),
				result:              BuildCachePreviewResultReady{value: preview},
				installMbxAvailable: installMbxAvailable,
			}
		} else {
			// The host may have changed even if its fresh preview failed.
			// Never let the pre-install result suppress a new resolution.
			delete(dialog.buildCachePreviews, machineID)
			notice += fmt.Sprintf(" Could not refresh the build cache preview: %s", previewErr.Error())
		}
	} else if previewErr != nil {
		notice += fmt.Sprintf(" Could not refresh the build cache preview: %s", previewErr.Error())
	}
	dialog.notice = &notice
	dialog.prepare()
}

// ArchiveSpacePreviewed takes the space an archive window would reclaim,
// dropping an answer the user has already typed past.
func (d *DashboardState) ArchiveSpacePreviewed(generation uint64, olderThanDays *uint32, preview core.ArchiveSpacePreview, err error) {
	dialog := setupDialogMut(d.mode)
	if dialog == nil {
		return
	}
	if dialog.generation != generation || dialog.archiveSpacePreview == nil ||
		!setupDaysEqual(dialog.archiveSpacePreview.key, olderThanDays) {
		return
	}
	// The estimate itself is drawn beside the value; the notice line only
	// carries a failure to measure.
	var result ArchiveSpacePreviewResult = ArchiveSpacePreviewResultReady{value: preview}
	if err != nil {
		notice := fmt.Sprintf("Could not measure what sessions use: %s", err.Error())
		dialog.notice = &notice
		result = ArchiveSpacePreviewResultFailed{value: err.Error()}
	}
	var key *uint32
	if olderThanDays != nil {
		days := *olderThanDays
		key = &days
	}
	dialog.archiveSpacePreview = &ArchiveSpacePreviewState{key: key, result: result}
	dialog.prepare()
}

// SetupPathResolved applies a target path's expanded home directory
// (resolved, or err).
func (d *DashboardState) SetupPathResolved(generation uint64, draft any, path []string, value string, resolved string, err error) {
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.generation != generation {
		return
	}
	pending, resolving := dialog.pendingSave.(PendingSaveResolving)
	resumesSave := resolving && slices.Equal(pending.path, path) && pending.value == value &&
		setupValueEqual(pending.draft, draft)
	if !setupValueEqual(dialog.draft, draft) {
		if resumesSave {
			dialog.pendingSave = PendingSaveIdle{}
		}
		return
	}
	field, ok := dialog.fields.get(path)
	if !ok || field.input.text().Value() != value {
		return
	}
	var applied error
	if err == nil {
		field.input.text().SetValue(resolved)
		applied = dialog.applyPending(path)
	} else {
		message := err.Error()
		field.err = &message
		applied = errors.New(message)
	}
	dialog.notice = nil
	if applied != nil {
		message := applied.Error()
		dialog.notice = &message
	}
	if resumesSave {
		if applied == nil {
			dialog.pendingSave = PendingSaveReady{}
		} else {
			dialog.pendingSave = PendingSaveIdle{}
			dialog.openPage(path[:len(path)-1])
			dialog.focusPath(path)
		}
	}
	dialog.form.RevealFocused()
}

// SetupLoginFinished reports the result of an interactive sign-in started
// from the profiles page. A dialog that closed while the command ran reports
// on the dashboard instead.
func (d *DashboardState) SetupLoginFinished(generation uint64, profileID string, result SetupLoginOutcome) {
	d.profileLoginRefresh = true
	var message string
	switch outcome := result.(type) {
	case SetupLoginOutcomeSigned:
		message = fmt.Sprintf("Signed in to %s. New and resumed sessions use the updated login.", profileID)
	case SetupLoginOutcomeCancelled:
		message = fmt.Sprintf("Sign-in to %s was cancelled.", profileID)
	case SetupLoginOutcomeFailed:
		message = fmt.Sprintf("Sign-in failed for %s: %s", profileID, outcome.Value)
	}
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.generation != generation {
		d.SetNotice(message)
		return
	}
	dialog.notice = &message
	dialog.prepare()
}

// SetupSaved applies a Settings save: the saved configuration, or err.
func (d *DashboardState) SetupSaved(generation uint64, config core.Config, err error) {
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.generation != generation {
		if err == nil {
			d.SetConfig(config)
		} else {
			d.SetFailureNotice(fmt.Sprintf("Could not save Settings: %s", err.Error()))
		}
		return
	}
	if err == nil {
		// Workers read the Jev switch when they start, so a session already
		// running keeps its old setting until then.
		jevChanged := !reflect.DeepEqual(config.Jev, d.config.Get().Jev)
		d.SetConfig(config)
		d.CancelModal()
		if jevChanged {
			d.SetNotice("Settings saved. New sessions use these defaults and the new Jev setting; running sessions follow it after their next resume or restart. Web listener changes apply on its next start.")
		} else {
			d.SetNotice("Settings saved. New sessions use these defaults. Web listener changes apply on its next start.")
		}
		return
	}
	if dialog := setupDialogMut(d.mode); dialog != nil {
		dialog.saving = false
		notice := fmt.Sprintf("Could not save: %s", err.Error())
		dialog.notice = &notice
		dialog.prepare()
	} else {
		d.SetFailureNotice(err.Error())
	}
}

// SetupDiscovered merges a detection run into the open Settings draft.
func (d *DashboardState) SetupDiscovered(generation uint64, detection SetupDetection, err error) {
	dialog := setupDialogMut(d.mode)
	if dialog == nil || dialog.generation != generation {
		return
	}
	dialog.discovering = false
	if err != nil {
		notice := fmt.Sprintf("Detection failed: %s", err.Error())
		dialog.notice = &notice
		dialog.prepare()
		return
	}
	scope := detection.Scope
	config := detection.Config
	rejectedRuntimes := slices.Clone(detection.RejectedRuntimes)
	// Reconcile against the current draft, including edits made while
	// discovery was running, rather than dropping collisions.
	current, currentErr := client.ConfigFromDraft(dialog.draft)
	if currentErr != nil {
		notice := fmt.Sprintf("Review the settings draft before detecting entries: %s", currentErr.Error())
		dialog.notice = &notice
		dialog.prepare()
		return
	}
	additions := current.SetupAdditions(config)
	localTargets := maps.Clone(additions.Targets)
	discovered, encodeErr := setupValueTo(additions)
	if encodeErr != nil {
		panic(fmt.Sprintf("config serializes: %v", encodeErr))
	}
	// A detection only writes its own section, so a profile run cannot add
	// machines and a runtime run cannot add projects.
	section := "profiles"
	if scope == DetectScopeRuntimes {
		section = "targets"
	}
	var added []string
	if entries, ok := setupValueObject(setupValueGet(discovered, section)); ok {
		for _, member := range entries {
			if setupValueHas(setupValueGet(dialog.draft, section), member.Key) {
				continue
			}
			setupValueInsert(&dialog.draft, []string{section}, member.Key, setupValueClone(member.Value))
			added = append(added, member.Key)
			if section == "profiles" {
				profileID := member.Key
				dialog.invalidateReviewValidationFor(&profileID)
			}
		}
	}
	// Each machine's runtimes are decided against the draft as it is now,
	// the same way a probe at startup decides against the file: a kind the
	// machine already runs is not added twice, and a machine edited while it
	// was probed gets nothing.
	foundOnMachines := false
	if section == "targets" {
		if current.Targets == nil {
			current.Targets = map[string]core.TargetTemplate{}
		}
		maps.Copy(current.Targets, localTargets)
		for _, probe := range detection.Machines {
			answered, isAnswered := probe.Finding.(client.MachineFindingAnswered)
			if !isAnswered {
				// The machine's own reason and fix stay separate fields
				// here: `finding_reason` writes a sentence that follows a
				// machine name and folds the fix back in, and the report
				// shows each alone.
				if unreachable, ok := probe.Finding.(client.MachineFindingUnreachable); ok {
					remediation := unreachable.Remediation
					rejectedRuntimes = append(rejectedRuntimes, client.RejectedRuntime{
						Kind:        "",
						Label:       probe.MachineID,
						Detail:      unreachable.Detail,
						Remediation: &remediation,
					})
				}
				continue
			}
			foundOnMachines = foundOnMachines || len(answered.Runtimes) > 0
			for _, runtime := range answered.Rejected {
				runtime.Label = fmt.Sprintf("%s on %s", runtime.Label, probe.MachineID)
				rejectedRuntimes = append(rejectedRuntimes, runtime)
			}
			runtimes := answered.Runtimes
			machineAdditions := current.MachineRuntimeAdditions(probe.MachineID, probe.Machine,
				func(yield func(string, core.TargetTemplate) bool) {
					for _, runtime := range runtimes {
						if !yield(runtime.First, runtime.Second) {
							return
						}
					}
				})
			if len(machineAdditions) == 0 {
				continue
			}
			maps.Copy(current.Targets, machineAdditions)
			// Stored against the draft's own machines, so each runtime names
			// the machine it was found on.
			stored := core.DefaultConfig()
			stored.Machines = maps.Clone(current.Machines)
			stored.Targets = machineAdditions
			storedValue, encodeErr := setupValueTo(stored)
			if encodeErr != nil {
				panic(fmt.Sprintf("config serializes: %v", encodeErr))
			}
			if entries, ok := setupValueObject(setupValueGet(storedValue, "targets")); ok {
				for _, member := range entries {
					setupValueInsert(&dialog.draft, []string{"targets"}, member.Key, member.Value)
					added = append(added, member.Key)
				}
			}
		}
	}
	root := []string{}
	client.Expand(&dialog.draft, &root)
	foundAny := len(config.Profiles) > 0
	if scope == DetectScopeRuntimes {
		foundAny = len(config.Targets) > 0 || foundOnMachines
	}
	notice := detectionNotice(scope, foundAny, added, rejectedRuntimes)
	for _, id := range slices.Sorted(maps.Keys(detection.ProfileReadiness)) {
		if missing := detection.ProfileReadiness[id]; missing != nil {
			notice += fmt.Sprintf(" %s: %s", id, *missing)
		}
	}
	dialog.notice = &notice
	// A runtimes run replaces the Detection results view's blocks; a
	// profiles run says nothing about runtimes, so it leaves the last report
	// alone until Settings closes.
	if scope == DetectScopeRuntimes {
		dialog.detectionRejected = rejectedRuntimes
	}
	dialog.prepare()
}

// machineNoteLines is how many lines a machine row's missing-runtime reason
// may take.
const machineNoteLines = 3

// machineWithoutRuntimeNote is why machine id in the Settings draft has no
// runtime, or false when a runtime in the draft runs on it. A machine as the
// configuration has it reports what its last check found; one the draft adds
// or changes reports what saving it will do.
func machineWithoutRuntimeNote(dashboard *DashboardState, dialog *SetupDialog, id string) (string, bool) {
	if id == core.LocalMachineID {
		return "", false
	}
	if targets, ok := setupValueObject(setupValueGet(dialog.draft, "targets")); ok {
		for _, member := range targets {
			if setupValueEqual(setupValueGet(member.Value, "machine"), id) {
				return "", false
			}
		}
	}
	saved := false
	if machine, err := setupValueMachine(setupValuePath(dialog.draft, "machines", id)); err == nil {
		if configured, ok := dashboard.config.Get().Machines[id]; ok && setupMachineEqual(configured, machine) {
			saved = true
		}
	}
	if !saved {
		return "Save it; opening New session then finds what it can run and adds those runtimes.", true
	}
	if reason, ok := dashboard.machineRuntimeReason(id); ok {
		return fmt.Sprintf("No runtime: %s", reason), true
	}
	return "Opening New session finds what it can run; Detect runtimes checks now.", true
}

// setupMachineEqual is Machine's derived PartialEq, compared through the
// serialized form both sides are read from.
func setupMachineEqual(a, b core.Machine) bool {
	left, errLeft := json.Marshal(a)
	right, errRight := json.Marshal(b)
	return errLeft == nil && errRight == nil && bytes.Equal(left, right)
}

// detectionNotice is what the Settings screen says after a detection run:
// the entries it added by name, and for runtimes how many it turned down,
// pointing at the Detection results view for each reason and fix. When it
// added nothing, it says whether it found nothing at all or found only what
// Settings already lists (launch finding R13-4).
func detectionNotice(scope DetectScope, foundAny bool, added []string, rejected []RejectedRuntime) string {
	var sentences []string
	if len(added) == 0 {
		switch {
		case scope == DetectScopeProfiles && !foundAny:
			sentences = append(sentences, fmt.Sprintf(
				"No coding agent installation was found on this machine. Add a profile, choose "+
					"Sign in, or install %s, sign in to it once, and choose Detect profiles again.",
				core.HarnessKindEveryDisplayNameOr(),
			))
		case scope == DetectScopeProfiles:
			sentences = append(sentences, "Every coding agent found on this machine already has a profile.")
		case !foundAny:
			sentences = append(sentences, "No usable runtime was found.")
		default:
			sentences = append(sentences, "Every usable runtime found is already listed.")
		}
	} else {
		names := strings.Join(added, ", ")
		if scope == DetectScopeProfiles {
			sentences = append(sentences, fmt.Sprintf("Added agent profiles: %s.", names))
		} else {
			sentences = append(sentences, fmt.Sprintf("Added runtimes: %s.", names))
		}
	}
	// Each refusal is a block in the Detection results view, never a
	// sentence here: a status band truncates, and the fix is the part that
	// must survive.
	if len(rejected) > 0 {
		sentences = append(sentences, fmt.Sprintf("%s left out; Detection results says why.",
			counted(len(rejected), "was", "were")))
	}
	if len(added) > 0 {
		sentences = append(sentences, "Review them, then Save.")
	}
	return strings.Join(sentences, " ")
}

// setupDialogMut is the open Settings dialog, or nil when mode is another
// one.
func setupDialogMut(mode Mode) *SetupDialog {
	if setup, ok := mode.(ModeSetup); ok {
		return setup.SetupDialog
	}
	return nil
}

// QueueSetupLogin holds a managed profile's login until the save result and
// updated configuration have been applied.
func (d *DashboardState) QueueSetupLogin(generation uint64, profileID string) {
	d.pendingSetupLogin = &client.DaemonPair[uint64, string]{First: generation, Second: profileID}
}

// TakeSetupLogin takes the queued managed-profile login.
func (d *DashboardState) TakeSetupLogin() (generation uint64, profileID string, ok bool) {
	pending := d.pendingSetupLogin
	if pending == nil {
		return 0, "", false
	}
	d.pendingSetupLogin = nil
	return pending.First, pending.Second, true
}
