// Port of mj-tui/src/setup/previews.rs.
//
// Rust's `pub(super) use mj_client::settings::schema::{...}` re-exports have
// no Go form: the setup files call client.BuildCacheGigabytesLabel,
// client.IsBuildCacheBudget and client.IsBuildCacheField directly.

package tui

import (
	"fmt"
	"maps"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func machineHostLabel(machineID string, machine core.Machine) string {
	switch machine := machine.(type) {
	case core.MachineLocal:
		return "the local machine"
	case core.MachineSsh:
		if machine.SSH.User != nil {
			return *machine.SSH.User + "@" + machine.SSH.Host
		}
		return machine.SSH.Host
	default:
		return machineID
	}
}

// formatMbxInstallResult is the install's own message, or what failed.
func formatMbxInstallResult(message string, err error, operation *MbxInstallState) string {
	if err == nil {
		return message
	}
	action := "install or upgrade"
	if operation != nil {
		action = "install"
		if operation.upgrading {
			action = "upgrade"
		}
	}
	return fmt.Sprintf("Could not %s mbx: %s", action, err.Error())
}

// formatCompilerTime is compiler time the cache avoided, in units somebody
// reads at a glance. The number is mbx's own estimate, so more precision
// than this would be false.
func formatCompilerTime(nanoseconds uint64) string {
	seconds := nanoseconds / 1_000_000_000
	switch {
	case seconds < 60:
		return fmt.Sprintf("%ds", seconds)
	case seconds < 3600:
		return fmt.Sprintf("%dm", seconds/60)
	case seconds < 86_400:
		return fmt.Sprintf("%dh %dm", seconds/3600, (seconds%3600)/60)
	default:
		return fmt.Sprintf("%dd %dh", seconds/86_400, (seconds%86_400)/3600)
	}
}

// buildCacheKey is the settings a machine's build cache preview depends on.
func (d *SetupDialog) buildCacheKey(machineID string) (any, bool) {
	machines := setupValueGet(d.draft, "machines")
	if !setupValueHas(d.draft, "machines") || !setupValueHas(machines, machineID) {
		return nil, false
	}
	return jsonvalue.Object{{Key: "machine", Value: setupValueClone(setupValueGet(machines, machineID))}}, true
}

// buildCacheKeyIs is `self.build_cache_key(machine_id).as_ref() == Some(key)`.
func (d *SetupDialog) buildCacheKeyIs(machineID string, key any) bool {
	current, ok := d.buildCacheKey(machineID)
	return ok && setupValueEqual(current, key)
}

func (d *SetupDialog) machineBuildCachePreview(machineID string) *core.BuildCachePreview {
	preview, ok := d.buildCachePreviews[machineID]
	if !ok || !d.buildCacheKeyIs(machineID, preview.key) {
		return nil
	}
	if ready, ok := preview.result.(BuildCachePreviewResultReady); ok {
		return ready.value
	}
	return nil
}

func (d *SetupDialog) buildCacheFieldUserManaged(path []string) bool {
	if !client.IsBuildCacheBudget(path) && !client.IsBuildCacheField(path, "directory") {
		return false
	}
	preview := d.machineBuildCachePreview(path[1])
	return preview != nil && preview.UserManaged
}

// reconcileHostOwnedFields: a late host reply can remove editability.
// Discard its pending text rather than leaving an invalid, unfocusable field
// blocking Save on another page.
func (d *SetupDialog) reconcileHostOwnedFields() {
	var owned [][]string
	for _, path := range d.fields.paths() {
		if d.buildCacheFieldUserManaged(path) {
			owned = append(owned, path)
		}
	}
	machines := map[string]struct{}{}
	for _, path := range owned {
		if field, ok := d.fields.remove(path); ok && field.input.text().Value() != field.applied {
			machines[path[1]] = struct{}{}
		}
		if resolving, ok := d.pendingSave.(PendingSaveResolving); ok && slices.Equal(resolving.path, path) {
			d.pendingSave = PendingSaveReady{}
		}
	}
	if len(machines) > 0 {
		notice := fmt.Sprintf(
			"Discarded pending build-cache edits for %s: this machine's mbx installation owns its budgets and directory. Mjolnir does not modify them.",
			strings.Join(slices.Sorted(maps.Keys(machines)), ", "),
		)
		d.notice = &notice
	}
}

// buildCachePage is the machine whose build cache page is showing, with the
// settings its preview depends on.
func (d *SetupDialog) buildCachePage() (string, any, bool) {
	if len(d.path) != 3 || d.path[0] != "machines" || d.path[2] != "build_cache" {
		return "", nil, false
	}
	key, ok := d.buildCacheKey(d.path[1])
	return d.path[1], key, ok
}

// buildCacheMachinePage is the machine page that holds the build cache row,
// whose summary depends on what the machine's host can do.
func (d *SetupDialog) buildCacheMachinePage() (string, any, bool) {
	if len(d.path) != 2 || d.path[0] != "machines" ||
		setupValuePath(d.draft, "machines", d.path[1], "build_cache") == nil {
		return "", nil, false
	}
	key, ok := d.buildCacheKey(d.path[1])
	return d.path[1], key, ok
}

// installMbxOffer is install eligibility and the exact machine draft a
// confirmation will use.
func (d *SetupDialog) installMbxOffer() (*MbxInstallOffer, bool) {
	machineID, key, ok := d.buildCacheMachinePage()
	if !ok {
		return nil, false
	}
	state, ok := d.buildCachePreviews[machineID]
	if !ok || !setupValueEqual(state.key, key) || !state.installMbxAvailable {
		return nil, false
	}
	ready, ok := state.result.(BuildCachePreviewResultReady)
	if !ok || ready.value == nil || ready.value.MbxProfileFile == nil {
		return nil, false
	}
	preview := ready.value
	machine, ok := d.machine(machineID)
	if !ok {
		return nil, false
	}
	return &MbxInstallOffer{
		machineID:      machineID,
		key:            key,
		upgrading:      preview.NativeMbx != nil,
		profileFile:    *preview.MbxProfileFile,
		profileWarning: preview.MbxProfileWarning,
		manualPathLine: preview.MbxManualPathLine,
		machine:        machine,
	}, true
}

// installMbxPageAction is the machine page's Install mbx button: its label
// and whether it is enabled.
func (d *SetupDialog) installMbxPageAction() (string, bool, bool) {
	if len(d.path) != 2 || d.path[0] != "machines" {
		return "", false, false
	}
	if install := d.mbxInstall; install != nil && install.machineID == d.path[1] {
		if install.upgrading {
			return "Upgrading mbx…", false, true
		}
		return "Installing mbx…", false, true
	}
	offer, ok := d.installMbxOffer()
	if !ok {
		return "", false, false
	}
	if offer.upgrading {
		return "Upgrade mbx", true, true
	}
	return "Install mbx", true, true
}

// buildCacheRowUnavailable is what the machine page's build cache row says
// when the resolved preview shows the host cannot share the cache.
func (d *SetupDialog) buildCacheRowUnavailable() (string, bool) {
	machineID, key, ok := d.buildCacheMachinePage()
	if !ok {
		return "", false
	}
	state, ok := d.buildCachePreviews[machineID]
	if !ok || !setupValueEqual(state.key, key) {
		return "", false
	}
	ready, ok := state.result.(BuildCachePreviewResultReady)
	if !ok || ready.value == nil {
		return "", false
	}
	switch ready.value.OffReason.(type) {
	case core.BuildCacheOffUnsupportedPlatform:
		// Only a Mac reports its own mbx while its containers cannot use it.
		if version := ready.value.NativeMbx; version != nil {
			return fmt.Sprintf("mbx %s, bare sessions only", *version), true
		}
		return "mbx unavailable", true
	case core.BuildCacheOffUnavailable:
		return "mbx unavailable", true
	default:
		return "", false
	}
}

// previewBuildCacheAction starts resolving the build cache page's automatic
// values on the target's host, unless the current settings are already
// resolved or in flight.
func (d *SetupDialog) previewBuildCacheAction() DashboardAction {
	machineID, key, ok := d.buildCachePage()
	if !ok {
		machineID, key, ok = d.buildCacheMachinePage()
	}
	if !ok {
		return DashboardActionNone{}
	}
	if preview, found := d.buildCachePreviews[machineID]; found && setupValueEqual(preview.key, key) {
		return DashboardActionNone{}
	}
	machine, err := setupValueMachine(setupValueGet(key, "machine"))
	if err != nil {
		// A draft that does not parse yet has nothing to resolve.
		return DashboardActionNone{}
	}
	d.buildCachePreviews[machineID] = BuildCachePreviewState{
		key:                 setupValueClone(key),
		result:              BuildCachePreviewResultResolving{},
		installMbxAvailable: false,
	}
	notice := "Resolving the build cache defaults on the machine…"
	d.notice = &notice
	return DashboardActionPreviewBuildCache{
		Generation: d.generation,
		Key:        key,
		Machine:    machine,
	}
}

// buildCacheAutomaticLabel is what the build cache page shows for a blank
// field: the value its host resolves, shown bare in place of the
// "automatic" placeholder.
func (d *SetupDialog) buildCacheAutomaticLabel(field string) (string, bool) {
	machineID, key, ok := d.buildCachePage()
	if !ok || field == "tools_directory" {
		return "", false
	}
	state, ok := d.buildCachePreviews[machineID]
	if !ok || !setupValueEqual(state.key, key) {
		return "", false
	}
	switch result := state.result.(type) {
	case BuildCachePreviewResultReady:
		if preview := result.value; preview != nil {
			switch field {
			case "enabled":
				if _, turnedOff := preview.OffReason.(core.BuildCacheOffTurnedOff); turnedOff {
					return chat.CheckboxMarker(false), true
				}
				return chat.CheckboxMarker(true), true
			case "directory":
				if preview.Directory != nil {
					return *preview.Directory, true
				}
				return "Unknown", true
			// Resolved sizes are shown in the same whole GB the field is
			// edited in.
			case "max_total_size":
				switch limit := preview.MaxTotalSize.(type) {
				case core.BuildCacheLimitSize:
					return fmt.Sprintf("%s GB, mj setting", client.BuildCacheGigabytesLabel(limit.Value)), true
				case core.BuildCacheLimitMjDefault:
					return fmt.Sprintf("%s GB, mj default", client.BuildCacheGigabytesLabel(limit.Value)), true
				case core.BuildCacheLimitHostConfiguration:
					switch {
					case limit.Value == nil:
						return "Unknown, host mbx config", true
					case *limit.Value == "none":
						return "Unlimited, host mbx", true
					default:
						return fmt.Sprintf("%s GB, host mbx config", client.BuildCacheGigabytesLabel(*limit.Value)), true
					}
				case core.BuildCacheLimitMbxDefault:
					if limit.Value == nil {
						return "Unlimited, mbx default", true
					}
					return fmt.Sprintf("%s GB, mbx default", client.BuildCacheGigabytesLabel(*limit.Value)), true
				default:
					return "Unknown", true
				}
			default:
				return "", false
			}
		}
		// The checkbox keeps showing the machine's own setting while the
		// other fields report how far the lookup got.
		if field == "enabled" {
			return "", false
		}
		return "Not available for this machine", true
	case BuildCachePreviewResultResolving:
		if field == "enabled" {
			return "", false
		}
		return "Resolving…", true
	case BuildCachePreviewResultFailed:
		if field == "enabled" {
			return "", false
		}
		return "Unknown", true
	}
	return "", false
}

func (d *SetupDialog) currentBuildCachePreview() *core.BuildCachePreview {
	machineID, _, ok := d.buildCachePage()
	if !ok {
		return nil
	}
	return d.machineBuildCachePreview(machineID)
}

func (d *SetupDialog) buildCacheUserManaged() bool {
	preview := d.currentBuildCachePreview()
	return preview != nil && preview.UserManaged
}

func (d *SetupDialog) buildCachePolicyLines() []string {
	preview := d.currentBuildCachePreview()
	// An unsupported host has no policy to apply.
	if preview == nil || preview.Directory == nil {
		return nil
	}
	owner := "Mj-managed build cache (mbx)"
	if preview.UserManaged {
		owner = "User-managed mbx; mj budgets are inactive"
	}
	var status string
	switch application := preview.Application.(type) {
	case core.BuildCacheApplicationPending:
		status = "Pending application"
	case core.BuildCacheApplicationApplied:
		status = "Applied to the machine"
	case core.BuildCacheApplicationFailed:
		status = "Application failed: " + application.Value
	}
	lines := []string{owner + " · " + status}
	if preview.BudgetNote != nil {
		lines = append(lines, *preview.BudgetNote)
	}
	return lines
}

// buildCacheStatsLine is what this machine's cache has actually done, for a
// line under the fields that configure it. The page otherwise only
// predicts.
func (d *SetupDialog) buildCacheStatsLine() (string, bool) {
	machineID, key, ok := d.buildCachePage()
	if !ok {
		return "", false
	}
	state, ok := d.buildCachePreviews[machineID]
	if !ok || !setupValueEqual(state.key, key) {
		return "", false
	}
	ready, ok := state.result.(BuildCachePreviewResultReady)
	if !ok || ready.value == nil || ready.value.Stats == nil {
		return "", false
	}
	stats := ready.value.Stats
	if stats.Builds == 0 {
		return "No build has used this cache yet.", true
	}
	return fmt.Sprintf("%d builds, %d compilations from cache, %s of compiler time saved, %s cloned",
		stats.Builds, stats.CachedCompilations, formatCompilerTime(stats.AvoidedCompilerNs),
		core.FormatResourceBytes(stats.ReflinkedBytes)), true
}

// mbxUnavailable: mbx availability does not govern native tool caches or
// their switch.
func (d *SetupDialog) mbxUnavailable() (string, bool) {
	machineID, key, ok := d.buildCachePage()
	if !ok {
		return "", false
	}
	state, ok := d.buildCachePreviews[machineID]
	if !ok || !setupValueEqual(state.key, key) {
		return "", false
	}
	ready, ok := state.result.(BuildCachePreviewResultReady)
	if !ok || ready.value == nil {
		return "", false
	}
	switch reason := ready.value.OffReason.(type) {
	case core.BuildCacheOffUnavailable:
		return reason.Value, true
	case core.BuildCacheOffUnsupportedPlatform:
		return reason.Value, true
	default:
		return "", false
	}
}

// archiveAfterDaysPage is the `archive_after_days` value the SessionWiki
// page is showing right now: the text being typed when that editor is open,
// otherwise the saved value. ok is false when no SessionWiki page is
// showing, so there is nothing to estimate; days is nil for no number.
func (d *SetupDialog) archiveAfterDaysPage() (days *uint32, ok bool) {
	if len(d.path) != 1 || d.path[0] != "sessionwiki" {
		return nil, false
	}
	if field, found := d.fields.get([]string{"sessionwiki", "archive_after_days"}); found {
		return setupParseDays(field.input.String()), true
	}
	// Edits store a number; a hand-written string is still read the same.
	value := setupValuePath(d.draft, "sessionwiki", "archive_after_days")
	if text, isText := setupValueString(value); isText {
		return setupParseDays(text), true
	}
	if number, isNumber := setupValueU64(value); isNumber && number <= uint64(^uint32(0)) {
		days := uint32(number)
		return &days, true
	}
	return nil, true
}

// setupParseDays is `text.trim().parse::<u32>().ok()`.
func setupParseDays(text string) *uint32 {
	trimmed := strings.TrimSpace(text)
	if strings.HasPrefix(trimmed, "-") {
		return nil
	}
	number, err := strconv.ParseUint(strings.TrimPrefix(trimmed, "+"), 10, 32)
	if err != nil {
		return nil
	}
	days := uint32(number)
	return &days
}

// previewArchiveSpaceAction starts measuring what the SessionWiki page's
// current archive window would reclaim, unless that value is already
// measured or in flight.
func (d *SetupDialog) previewArchiveSpaceAction() DashboardAction {
	olderThanDays, ok := d.archiveAfterDaysPage()
	if !ok {
		return DashboardActionNone{}
	}
	if d.archiveSpacePreview != nil && setupDaysEqual(d.archiveSpacePreview.key, olderThanDays) {
		return DashboardActionNone{}
	}
	d.archiveSpacePreview = &ArchiveSpacePreviewState{
		key:    olderThanDays,
		result: ArchiveSpacePreviewResultResolving{},
	}
	return DashboardActionPreviewArchiveSpace{
		Generation:    d.generation,
		OlderThanDays: olderThanDays,
	}
}

// archiveSpaceAutomaticLabel is what the SessionWiki page shows in the value
// column of "Archive after (days)": the value, then the space sessions use
// today and what that value would reclaim. Replaces the plain number, not
// just the placeholder.
func (d *SetupDialog) archiveSpaceAutomaticLabel(field string) (string, bool) {
	if field != "archive_after_days" {
		return "", false
	}
	days, ok := d.archiveAfterDaysPage()
	if !ok {
		return "", false
	}
	estimate, ok := d.archiveSpaceEstimate()
	if !ok {
		return "", false
	}
	if days == nil {
		return estimate, true
	}
	return fmt.Sprintf("%d · %s", *days, estimate), true
}

// archiveSpaceEstimate is the estimate for the archive window the
// SessionWiki page is showing, without the value itself: under the open
// editor the value is the text being typed, so repeating it would only add
// noise.
func (d *SetupDialog) archiveSpaceEstimate() (string, bool) {
	days, ok := d.archiveAfterDaysPage()
	if !ok || d.archiveSpacePreview == nil || !setupDaysEqual(d.archiveSpacePreview.key, days) {
		return "", false
	}
	switch result := d.archiveSpacePreview.result.(type) {
	case ArchiveSpacePreviewResultResolving:
		return "Resolving…", true
	case ArchiveSpacePreviewResultFailed:
		return "Unknown", true
	case ArchiveSpacePreviewResultReady:
		preview := result.value
		if days == nil {
			return "Never · sessions use " + core.FormatResourceBytes(preview.Bytes), true
		}
		return fmt.Sprintf("would reclaim %s of %s (%d of %d sessions)",
			core.FormatResourceBytes(preview.ReclaimableBytes), core.FormatResourceBytes(preview.Bytes),
			preview.ReclaimableSessions, preview.Sessions), true
	}
	return "", false
}

// setupDaysEqual is Option<u32> equality.
func setupDaysEqual(a, b *uint32) bool {
	return (a == nil) == (b == nil) && (a == nil || *a == *b)
}
