// Port of mj-tui/src/wizards.rs.

// New-session and resume wizards, including their mount and review steps.
//
// The submodules (wizards/picker.rs, projects.rs, render.rs, resources.rs,
// subagents.rs, dashboard/**, draft.rs) are the wizards_*.go files of this
// package.

package tui

import (
	"slices"
	"strings"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// wizardModal keeps the frame and action row in place while moving between
// wizard steps.
func wizardModal(frame *chat.Frame, surfaces *chat.FrameSurfaces, area uv.Rectangle) uv.Rectangle {
	return chat.CenteredModalFixed(frame, surfaces, 84, 24, area)
}

// WizardStep is one step of the new-session and resume wizards.
type WizardStep int

const (
	WizardStepProfile WizardStep = iota
	WizardStepTarget
	WizardStepBundle
	WizardStepProjectDirectory
	WizardStepReview
	// WizardStepLaunching is where automatic submission waits for
	// supervised prerequisite checks.
	WizardStepLaunching
	WizardStepMounts
	WizardStepNewBundle
)

// WizardControl is a stable control identity shared by the new-session and
// resume wizards. Every control is a constant except RecentProject(index),
// which [WizardControlRecentProject] builds and
// [WizardControl.RecentProjectIndex] reads back.
type WizardControl int

const (
	WizardControlProfileList WizardControl = iota
	WizardControlBundleList
	WizardControlTargetList
	WizardControlResourceCpu
	WizardControlResourceMemory
	WizardControlRememberContainerSize
	WizardControlProjectDirectory
	WizardControlNewBundleRepositories
	WizardControlNewBundleSource
	WizardControlNewBundleRemove
	WizardControlRemoveBundle
	WizardControlProjectRecent
	WizardControlProjectGithub
	WizardControlProjectUrl
	WizardControlProjectQuery
	WizardControlProjectSearch
	WizardControlProjectResults
	WizardControlProjectRetry
	WizardControlProjectMultiple
	WizardControlProjectMakePrimary
	WizardControlMountSource
	WizardControlMountDestination
	WizardControlMountAccess
	WizardControlMountHistory
	WizardControlReviewAttachments
	WizardControlSubagents
	WizardControlOracle
	WizardControlOrchestrators
	WizardControlCreateManagedWorktree
	WizardControlUseSandboxedTarget
	WizardControlDiscardQueue
	WizardControlCancel
	WizardControlBack
	WizardControlNext
	WizardControlAdd
	WizardControlSubmit

	// wizardControlRecentProjectBase is RecentProject(0); RecentProject(n)
	// is the base plus n.
	wizardControlRecentProjectBase WizardControl = 1 << 16
)

// WizardControlRecentProject is WizardControl::RecentProject(index).
func WizardControlRecentProject(index int) WizardControl {
	return wizardControlRecentProjectBase + WizardControl(index)
}

// RecentProjectIndex reads back RecentProject(index).
func (c WizardControl) RecentProjectIndex() (int, bool) {
	if c < wizardControlRecentProjectBase {
		return 0, false
	}
	return int(c - wizardControlRecentProjectBase), true
}

// ProjectDirectoryCheck is what is known about one configured project
// directory.
type ProjectDirectoryCheck struct {
	state      ProjectDirectoryState
	recordedAt time.Time
}

// ProjectDirectoryState is the answer a project directory check gave.
type ProjectDirectoryState int

const (
	// ProjectDirectoryStateChecking is in flight, and never re-requested.
	ProjectDirectoryStateChecking ProjectDirectoryState = iota
	ProjectDirectoryStatePresent
	ProjectDirectoryStateMissing
	// ProjectDirectoryStateUnknown means the check could not tell, so the
	// picker says nothing about it.
	ProjectDirectoryStateUnknown
)

// projectDirectoryCheckTTL: a directory that was missing may come back, and
// one that was there may go, so answers are re-checked on a later visit to
// the project step.
const projectDirectoryCheckTTL = 60 * time.Second

// WorktreeDirectoryCheck is the background check of a bare project
// directory (Rust's `(String, String, ManagedWorktreeOptions, bool)`): the
// target and directory it ran against, the checkout options, and whether
// creation will turn review off.
type WorktreeDirectoryCheck struct {
	TargetID       string
	Directory      string
	Options        core.ManagedWorktreeOptions
	ReviewTurnsOff bool
}

// NewWizard is the new-session wizard's draft.
type NewWizard struct {
	subagents *SubagentDraft
	// worktreeOptions is the background check of the bare project
	// directory, for the target and directory it ran against.
	worktreeOptions       *WorktreeDirectoryCheck
	createManagedWorktree bool
	// workspaceID keeps creation in this workspace even if the visible tab
	// changes.
	workspaceID string
	step        WizardStep

	profile           int
	expandedProviders map[core.QuotaProvider]struct{}
	// focusedProvider is nil when no provider header has focus.
	focusedProvider core.QuotaProvider
	bundle          int
	target          int
	// targetStepSkipped says the draft passed the target step without
	// showing it, because that step offered one target and nothing else to
	// decide. Back and the step numbers in the title then leave that step
	// out.
	targetStepSkipped  bool
	profileStepSkipped bool
	mounts             MountWizard

	projectPicker          *ProjectPicker
	newBundleSelected      int
	newBundleRepositories  []string
	newBundleSource        chat.PathInput
	bundleCreationInFlight bool
	// bundleRemovalInFlight: removing a saved project rewrites the config,
	// so the project list is frozen until the runtime feed shows it gone.
	bundleRemovalInFlight bool
	// bundleRemovalOutcome is the last removal refusal or failure, shown
	// inside the project picker.
	bundleRemovalOutcome  *string
	projectDirectory      chat.PathInput
	projectDirectoryError *string
	projectHistory        []string
	projectHistoryIndex   int
	// resourceAllocation is nil for fixed or default resources.
	resourceAllocation    core.SessionResourceAllocation
	rememberContainerSize bool
	sizingError           *string
	resourceEditor        ResourceEditor
	// remoteRepositories are the network clone destinations returned by the
	// asynchronous creation preflight. A nonempty value is concrete evidence
	// the review is ready; nil means no answer yet.
	remoteRepositories *[]RemoteRepositoryPreview
	// remotePreflightInFlight covers the whole prerequisite check:
	// attached-directory validation, then network source resolution.
	remotePreflightInFlight bool
	remotePreflightError    *string
	form                    chat.Dialog[WizardControl]
}

// ProfilePickerItem is one row of the new-session profile picker:
// [ProfilePickerItemAuto], [ProfilePickerItemProvider], or
// [ProfilePickerItemProfile].
type ProfilePickerItem interface {
	isProfilePickerItem()
}

type ProfilePickerItemAuto struct{}

type ProfilePickerItemProvider struct {
	Provider core.QuotaProvider
}

type ProfilePickerItemProfile struct {
	ProfileIndex int
	ID           string
}

func (ProfilePickerItemAuto) isProfilePickerItem()     {}
func (ProfilePickerItemProvider) isProfilePickerItem() {}
func (ProfilePickerItemProfile) isProfilePickerItem()  {}

func newWizardProfileItems(dashboard *DashboardState, expanded map[core.QuotaProvider]struct{}) []ProfilePickerItem {
	panic("unported: mj-tui/src/wizards.rs new_wizard_profile_items")
}

func (w *NewWizard) toggleProvider(provider core.QuotaProvider) {
	if _, expanded := w.expandedProviders[provider]; expanded {
		delete(w.expandedProviders, provider)
		return
	}
	if w.expandedProviders == nil {
		w.expandedProviders = make(map[core.QuotaProvider]struct{})
	}
	w.expandedProviders[provider] = struct{}{}
}

// newWizardProviderForProfile is NewWizard::provider_for_profile: the
// provider of the profile row at profileIndex, nil for the Auto row.
func newWizardProviderForProfile(dashboard *DashboardState, profileIndex int) core.QuotaProvider {
	panic("unported: mj-tui/src/wizards.rs NewWizard::provider_for_profile")
}

// effectiveFocusedProvider is the focused provider header if it still
// exists among offered profiles. Asynchronous provider discovery can
// replace a `(resolving)` group with the resolved provider; clearing a stale
// focus keeps the picker in sync.
func (w *NewWizard) effectiveFocusedProvider(dashboard *DashboardState) core.QuotaProvider {
	panic("unported: mj-tui/src/wizards.rs NewWizard::effective_focused_provider")
}

// newWizardIsResolvingProvider is NewWizard::is_resolving_provider.
func newWizardIsResolvingProvider(provider core.QuotaProvider) bool {
	other, ok := provider.(core.QuotaProviderOther)
	return ok && strings.HasSuffix(other.Value, " (resolving)")
}

func (w *NewWizard) selectedProvider(dashboard *DashboardState) core.QuotaProvider {
	// On later steps, retain the explicit provider choice even if its last
	// profile disappears so creation reports the unavailability rather than
	// silently falling back to another profile.
	focused := w.focusedProvider
	if w.step == WizardStepProfile {
		focused = w.effectiveFocusedProvider(dashboard)
	}
	if focused == nil || newWizardIsResolvingProvider(focused) {
		return nil
	}
	return focused
}

func (w *NewWizard) selectedProfileID(dashboard *DashboardState) (string, bool) {
	panic("unported: mj-tui/src/wizards.rs NewWizard::selected_profile_id")
}

func (w *NewWizard) selectedProfileLabel(dashboard *DashboardState) string {
	if provider := w.selectedProvider(dashboard); provider != nil {
		return "Auto (" + provider.DisplayName() + ")"
	}
	return dashboard.newWizardProfileLabel(w.profile)
}

// effectiveExpandedProviders are the expanded provider groups for rendering
// and event handling. Whenever focus is on a concrete profile rather than a
// provider header, its current provider is expanded so the selected profile
// is never hidden after asynchronous provider resolution.
func (w *NewWizard) effectiveExpandedProviders(dashboard *DashboardState) map[core.QuotaProvider]struct{} {
	panic("unported: mj-tui/src/wizards.rs NewWizard::effective_expanded_providers")
}

// equal is Rust's PartialEq for NewWizard: every field but the form.
func (w *NewWizard) equal(other *NewWizard) bool {
	panic("unported: mj-tui/src/wizards.rs NewWizard::eq")
}

// MountWizard is the attachment editor shared by both wizards.
type MountWizard struct {
	source      chat.PathInput
	destination chat.PathInput

	access           core.MountAccess
	accessCombo      chat.ComboBoxState[WizardControl]
	mounts           []core.AdditionalMount
	history          []string
	selectedMount    int
	historySelection int
	// forcedSources are the sources the target's host reported as unable to
	// hold Podman's overlay, keyed by the typed source and holding the
	// `filesystem (reason)` label.
	forcedSources map[string]string
	error         *string
	editingMount  *int
}

func newMountWizard(history []string) MountWizard {
	return MountWizard{
		source:        chat.NewPathInput(),
		destination:   chat.NewPathInput(),
		access:        core.MountAccessRo,
		history:       history,
		forcedSources: make(map[string]string),
	}
}

// mountWizardWithMounts is MountWizard::with_mounts.
func mountWizardWithMounts(history []string, mounts []core.AdditionalMount) MountWizard {
	wizard := newMountWizard(history)
	wizard.mounts = mounts
	return wizard
}

// overlayUnavailable is why the source under edit cannot use the
// copy-on-write overlay, if it cannot.
func (m *MountWizard) overlayUnavailable() (string, bool) {
	reason, ok := m.forcedSources[strings.TrimSpace(m.source.Value())]
	return reason, ok
}

// accessChoices are the access modes the entry under edit may use.
func (m *MountWizard) accessChoices() []core.MountAccess {
	_, forced := m.overlayUnavailable()
	return accessChoices(forced)
}

// forbidOverlay settles the access mode after the host reported the overlay
// unusable.
func (m *MountWizard) forbidOverlay() {
	m.access = m.access.WithoutOverlay()
}

func (m *MountWizard) availableHistory() []string {
	panic("unported: mj-tui/src/wizards.rs MountWizard::available_history")
}

func (m *MountWizard) addValidatedMount() {
	panic("unported: mj-tui/src/wizards.rs MountWizard::add_validated_mount")
}

// equal is Rust's derived PartialEq for MountWizard.
func (m *MountWizard) equal(other *MountWizard) bool {
	panic("unported: mj-tui/src/wizards.rs MountWizard::eq")
}

func (w *NewWizard) selectedWorktreeOptions(config *core.Config) (core.ManagedWorktreeOptions, bool) {
	options, _, ok := w.selectedDirectoryCheck(config)
	return options, ok
}

// selectedReviewTurnsOff reports whether creating this session turns its
// review off.
func (w *NewWizard) selectedReviewTurnsOff(config *core.Config) bool {
	_, reviewTurnsOff, ok := w.selectedDirectoryCheck(config)
	return ok && reviewTurnsOff
}

// selectedDirectoryCheck is the directory check, only while it still
// matches the selection.
func (w *NewWizard) selectedDirectoryCheck(config *core.Config) (core.ManagedWorktreeOptions, bool, bool) {
	panic("unported: mj-tui/src/wizards.rs NewWizard::selected_directory_check")
}

func (w *NewWizard) prepareDialogState() {
	panic("unported: mj-tui/src/wizards.rs NewWizard::prepare_dialog_state")
}

func (w *ResumeWizard) prepareDialogState() {
	panic("unported: mj-tui/src/wizards.rs ResumeWizard::prepare_dialog_state")
}

func (w *ResumeWizard) hasQueuedWork(dashboard *DashboardState) bool {
	panic("unported: mj-tui/src/wizards.rs ResumeWizard::has_queued_work")
}

// ResumeSource is what the resume wizard will start.
//
// The controls are the same either way — a profile, a target, attachments —
// but a record is resumed from its checkpoint while an archived SessionWiki
// session has no record and no checkpoint and is restored into a brand-new
// session carrying a summary of its transcript.
type ResumeSource int

const (
	ResumeSourceSession ResumeSource = iota
	ResumeSourceArchive
)

// ResumeWizard is the resume and move wizard's draft.
type ResumeWizard struct {
	subagents *SubagentDraft
	// sessionID is the Mjolnir session being resumed or moved, or, for an
	// archive, the SessionWiki id being restored. source says which.
	sessionID string
	source    ResumeSource
	// title is shown in the wizard's title for an archive, which has no
	// record to read a title from.
	title string
	// workspaceID keeps resuming in the workspace where the dialog was
	// opened even if the visible tab changes before submission.
	workspaceID string
	// moving: the resume form is also the move form. Move keeps the source
	// workspace fixed and submits a daemon Move request instead of a plain
	// resume.
	moving      bool
	preparation *core.MovePreparation
	preparing   bool
	// preparationRequestID is the identity of the preparation request
	// currently owned by this wizard. It is cleared whenever the draft
	// changes or the wizard leaves review.
	preparationRequestID *uint64
	// preparationError is a failed preparation, which remains visible in the
	// review modal so retry is an explicit, single action.
	preparationError *string
	preflightError   *string
	step             WizardStep

	profile int
	target  int
	// targetStepSkipped says the draft passed the target step without
	// showing it, because that step offered one target and nothing else to
	// decide. Back and the step numbers in the title then leave that step
	// out.
	targetStepSkipped  bool
	profileStepSkipped bool
	mounts             MountWizard

	resourceAllocation core.SessionResourceAllocation
	sizingError        *string
	resourceEditor     ResourceEditor
	discardQueue       bool
	form               chat.Dialog[WizardControl]
}

// equal is Rust's PartialEq for ResumeWizard: every field but the form.
func (w *ResumeWizard) equal(other *ResumeWizard) bool {
	panic("unported: mj-tui/src/wizards.rs ResumeWizard::eq")
}

func removeSelectedMount(mounts *MountWizard) {
	if len(mounts.mounts) == 0 {
		return
	}
	mounts.mounts = append(mounts.mounts[:mounts.selectedMount], mounts.mounts[mounts.selectedMount+1:]...)
	mounts.historySelection = 0
	mounts.selectedMount = min(mounts.selectedMount, max(len(mounts.mounts)-1, 0))
}

// prepareMountEditor empties the attachment editor for a new entry. The
// caller moves the wizard to [WizardStepMounts].
func prepareMountEditor(mounts *MountWizard) {
	mounts.source.Clear()
	mounts.destination.Clear()
	mounts.historySelection = 0
	mounts.access = core.MountAccessRo
	mounts.error = nil
	mounts.editingMount = nil
	mounts.source.DismissCompletion()
}

// prepareSelectedMountEditor loads the selected attachment into the editor.
// It answers false when there is nothing to edit, in which case the wizard
// must stay on its current step.
func prepareSelectedMountEditor(mounts *MountWizard) bool {
	panic("unported: mj-tui/src/wizards.rs prepare_selected_mount_editor")
}

func beginMountEditor(wizard WizardDraft) {
	prepareMountEditor(wizard.MountsMut())
	wizard.SetStep(WizardStepMounts)
	openMountEditor(wizard)
}

func editSelectedMount(wizard WizardDraft) {
	if prepareSelectedMountEditor(wizard.MountsMut()) {
		wizard.SetStep(WizardStepMounts)
	}
	openMountEditor(wizard)
}

func openMountEditor(wizard WizardDraft) {
	form := wizard.FormMut()
	form.ForgetDraftPart("attachment editor")
	form.Focus(WizardControlMountSource)
}

// validateMountEntry is why the entry under edit cannot be attached, or
// false when it can.
func validateMountEntry(mounts *MountWizard) (string, bool) {
	panic("unported: mj-tui/src/wizards.rs validate_mount_entry")
}

func (w *NewWizard) addNewBundleRepository() bool {
	panic("unported: mj-tui/src/wizards.rs NewWizard::add_new_bundle_repository")
}

func (w *NewWizard) removeSelectedNewBundleRepository() bool {
	panic("unported: mj-tui/src/wizards.rs NewWizard::remove_selected_new_bundle_repository")
}

func (w *NewWizard) newBundleSourcesForSubmit() []string {
	sources := append([]string(nil), w.newBundleRepositories...)
	current := strings.TrimSpace(w.newBundleSource.Value())
	if current != "" && !slices.Contains(sources, current) {
		sources = append(sources, current)
	}
	return sources
}

func nthBundleKey(config *core.Config, recency []client.LaunchRecency, index int) string {
	ids := bundleIDsByRecentCreation(config, recency)
	if index < 0 || index >= len(ids) {
		panic("wizard is only opened for non-empty configuration")
	}
	return ids[index]
}

// mostRecentConfiguredSession is the newest launch whose profile, project
// and target are all still configured. New-session defaults start from it.
func mostRecentConfiguredSession(config *core.Config, recency []client.LaunchRecency) *client.LaunchRecency {
	panic("unported: mj-tui/src/wizards.rs most_recent_configured_session")
}

// createdAtMs is created_at as Unix milliseconds, when it parses as RFC
// 3339.
func createdAtMs(createdAt string) (int64, bool) {
	timestamp, err := time.Parse(time.RFC3339, createdAt)
	if err != nil {
		return 0, false
	}
	return timestamp.UnixMilli(), true
}

func bundleIDsByRecentCreation(config *core.Config, recency []client.LaunchRecency) []string {
	panic("unported: mj-tui/src/wizards.rs bundle_ids_by_recent_creation")
}

func targetLabel(target core.TargetTemplate) string {
	panic("unported: mj-tui/src/wizards.rs target_label")
}

// resourceAllocationDescription is the compute a session gets, on its own,
// as the review's Compute row shows it. A nil allocation is fixed or
// default resources.
func resourceAllocationDescription(allocation core.SessionResourceAllocation) string {
	panic("unported: mj-tui/src/wizards.rs resource_allocation_description")
}

func defaultResourceDestination(target core.TargetTemplate, source string, existing []core.AdditionalMount) string {
	panic("unported: mj-tui/src/wizards.rs default_resource_destination")
}
