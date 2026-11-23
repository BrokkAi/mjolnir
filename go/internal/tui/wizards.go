// Port of mj-tui/src/wizards.rs.

// New-session and resume wizards, including their mount and review steps.
//
// The submodules (wizards/picker.rs, projects.rs, render.rs, resources.rs,
// subagents.rs, dashboard/**, draft.rs) are the wizards_*.go files of this
// package.

package tui

import (
	"cmp"
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strconv"
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
	hasAuto := dashboard.newWizardHasAutoPrimary()
	var items []ProfilePickerItem
	if hasAuto {
		items = append(items, ProfilePickerItemAuto{})
	}
	type member struct {
		profileIndex int
		id           string
	}
	var providers []core.QuotaProvider
	groups := map[core.QuotaProvider][]member{}
	index := 0
	for id, profile := range dashboard.offeredProfiles() {
		provider := profileProvider(id, &profile, &dashboard.quotaProviders)
		if _, seen := groups[provider]; !seen {
			providers = append(providers, provider)
		}
		groups[provider] = append(groups[provider], member{profileIndex: index + wizardBoolIndex(hasAuto), id: id})
		index++
	}
	// Rust walks the groups as a BTreeMap, in provider order.
	slices.SortFunc(providers, compareQuotaProviders)
	for _, provider := range providers {
		items = append(items, ProfilePickerItemProvider{Provider: provider})
		if _, open := expanded[provider]; open {
			for _, member := range groups[provider] {
				items = append(items, ProfilePickerItemProfile{ProfileIndex: member.profileIndex, ID: member.id})
			}
		}
	}
	return items
}

// wizardBoolIndex is Rust's `usize::from(bool)`.
func wizardBoolIndex(value bool) int {
	if value {
		return 1
	}
	return 0
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
	hasAuto := dashboard.newWizardHasAutoPrimary()
	if hasAuto && profileIndex == 0 {
		return nil
	}
	offeredIndex := profileIndex - wizardBoolIndex(hasAuto)
	if offeredIndex < 0 {
		return nil
	}
	index := 0
	for id, profile := range dashboard.offeredProfiles() {
		if index == offeredIndex {
			return profileProvider(id, &profile, &dashboard.quotaProviders)
		}
		index++
	}
	return nil
}

// effectiveFocusedProvider is the focused provider header if it still
// exists among offered profiles. Asynchronous provider discovery can
// replace a `(resolving)` group with the resolved provider; clearing a stale
// focus keeps the picker in sync.
func (w *NewWizard) effectiveFocusedProvider(dashboard *DashboardState) core.QuotaProvider {
	focused := w.focusedProvider
	if focused == nil {
		return nil
	}
	for id, profile := range dashboard.offeredProfiles() {
		if profileProvider(id, &profile, &dashboard.quotaProviders) == focused {
			return focused
		}
	}
	return nil
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
	if w.selectedProvider(dashboard) != nil {
		return "", true
	}
	return dashboard.newWizardProfileID(w.profile)
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
	expanded := maps.Clone(w.expandedProviders)
	if expanded == nil {
		expanded = map[core.QuotaProvider]struct{}{}
	}
	if w.effectiveFocusedProvider(dashboard) == nil {
		if provider := newWizardProviderForProfile(dashboard, w.profile); provider != nil {
			expanded[provider] = struct{}{}
		}
	}
	return expanded
}

// equal is Rust's PartialEq for NewWizard: every field but the form.
func (w *NewWizard) equal(other *NewWizard) bool {
	return wizardSubagentDraftsEqual(w.subagents, other.subagents) &&
		wizardOptionalEqual(w.worktreeOptions, other.worktreeOptions) &&
		w.createManagedWorktree == other.createManagedWorktree &&
		w.workspaceID == other.workspaceID &&
		w.step == other.step &&
		w.profile == other.profile &&
		maps.Equal(w.expandedProviders, other.expandedProviders) &&
		w.focusedProvider == other.focusedProvider &&
		w.bundle == other.bundle &&
		w.target == other.target &&
		w.profileStepSkipped == other.profileStepSkipped &&
		w.targetStepSkipped == other.targetStepSkipped &&
		w.mounts.equal(&other.mounts) &&
		w.projectPicker.equal(other.projectPicker) &&
		w.newBundleSelected == other.newBundleSelected &&
		slices.Equal(w.newBundleRepositories, other.newBundleRepositories) &&
		w.newBundleSource.Equal(&other.newBundleSource) &&
		w.bundleCreationInFlight == other.bundleCreationInFlight &&
		w.bundleRemovalInFlight == other.bundleRemovalInFlight &&
		wizardOptionalStringsEqual(w.bundleRemovalOutcome, other.bundleRemovalOutcome) &&
		w.projectDirectory.Equal(&other.projectDirectory) &&
		wizardOptionalStringsEqual(w.projectDirectoryError, other.projectDirectoryError) &&
		slices.Equal(w.projectHistory, other.projectHistory) &&
		w.projectHistoryIndex == other.projectHistoryIndex &&
		w.resourceAllocation == other.resourceAllocation &&
		w.rememberContainerSize == other.rememberContainerSize &&
		wizardOptionalStringsEqual(w.sizingError, other.sizingError) &&
		w.resourceEditor.Equal(&other.resourceEditor) &&
		wizardRemoteRepositoriesEqual(w.remoteRepositories, other.remoteRepositories) &&
		w.remotePreflightInFlight == other.remotePreflightInFlight &&
		wizardOptionalStringsEqual(w.remotePreflightError, other.remotePreflightError)
}

// wizardOptionalEqual compares two Rust `Option<T>` values of a comparable T.
func wizardOptionalEqual[T comparable](left, right *T) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

// wizardRemoteRepositoriesEqual compares two `Option<Vec<RemoteRepositoryPreview>>`
// values.
func wizardRemoteRepositoriesEqual(left, right *[]RemoteRepositoryPreview) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return slices.EqualFunc(*left, *right, func(a, b RemoteRepositoryPreview) bool {
		return a.RepositoryID == b.RepositoryID && a.FetchURL == b.FetchURL &&
			a.DefaultBranch == b.DefaultBranch && slices.Equal(a.PushURLs, b.PushURLs)
	})
}

// wizardSubagentDraftsEqual is SubagentDraft's derived PartialEq.
func wizardSubagentDraftsEqual(left, right *SubagentDraft) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return left.oracle == right.oracle &&
		wizardSubagentChoicesEqual(left.oracleChoice, right.oracleChoice) &&
		left.oracleConfigured == right.oracleConfigured &&
		left.orchestratorsConfigured == right.orchestratorsConfigured &&
		wizardSubagentPoliciesEqual(&left.policy, &right.policy) &&
		wizardOptionalSubagentPoliciesEqual(left.chosenPolicy, right.chosenPolicy) &&
		left.hasRememberedPolicy == right.hasRememberedPolicy &&
		wizardOptionalSubagentPoliciesEqual(left.lastMjolnirPolicy, right.lastMjolnirPolicy) &&
		left.combo == right.combo &&
		reflect.DeepEqual(left.discovery, right.discovery) &&
		wizardOptionalEqual(left.harness, right.harness) &&
		wizardOptionalStringsEqual(left.profileID, right.profileID)
}

func wizardSubagentChoicesEqual(left, right *core.SubagentChoice) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return left.Model == right.Model && wizardOptionalStringsEqual(left.Effort, right.Effort)
}

func wizardSubagentPoliciesEqual(left, right *core.SubagentPolicy) bool {
	return left.Mode == right.Mode &&
		wizardSubagentChoicesEqual(left.Straightforward, right.Straightforward) &&
		wizardSubagentChoicesEqual(left.Subtle, right.Subtle) &&
		left.Orchestrators == right.Orchestrators
}

func wizardOptionalSubagentPoliciesEqual(left, right *core.SubagentPolicy) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return wizardSubagentPoliciesEqual(left, right)
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
	var available []string
	for _, source := range m.history {
		taken := false
		for index, mount := range m.mounts {
			if (m.editingMount == nil || *m.editingMount != index) && mount.Source == source {
				taken = true
				break
			}
		}
		if !taken {
			available = append(available, source)
		}
	}
	return available
}

func (m *MountWizard) addValidatedMount() {
	mount := core.AdditionalMount{
		Source:      m.source.String(),
		Destination: m.destination.String(),
		Access:      m.access,
	}
	if m.editingMount != nil {
		index := *m.editingMount
		m.editingMount = nil
		m.mounts[index] = mount
	} else {
		m.mounts = append(m.mounts, mount)
	}
	m.source.Clear()
	m.destination.Clear()
	m.historySelection = 0
	m.access = core.MountAccessRo
	m.source.DismissCompletion()
	m.error = nil
}

// equal is Rust's derived PartialEq for MountWizard.
func (m *MountWizard) equal(other *MountWizard) bool {
	return m.source.Equal(&other.source) &&
		m.destination.Equal(&other.destination) &&
		m.access == other.access &&
		m.accessCombo == other.accessCombo &&
		slices.Equal(m.mounts, other.mounts) &&
		slices.Equal(m.history, other.history) &&
		m.selectedMount == other.selectedMount &&
		m.historySelection == other.historySelection &&
		maps.Equal(m.forcedSources, other.forcedSources) &&
		wizardOptionalStringsEqual(m.error, other.error) &&
		wizardOptionalEqual(m.editingMount, other.editingMount)
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
	targetID := nthKey(config.Targets, w.target)
	check := w.worktreeOptions
	if check == nil || check.TargetID != targetID || check.Directory != strings.TrimSpace(w.projectDirectory.Value()) {
		return core.ManagedWorktreeOptions{}, false, false
	}
	return check.Options, check.ReviewTurnsOff, true
}

func (w *NewWizard) prepareDialogState() {
	form := &w.form
	form.SetDismissActions([]WizardControl{WizardControlCancel, WizardControlBack})
	var escape *WizardControl
	if w.step == WizardStepMounts || w.step == WizardStepNewBundle {
		back := WizardControlBack
		escape = &back
	}
	form.SetEscapeAction(escape)
	form.SetDismissalScope(WizardControlBack, "attachment editor")
	form.TrackDraftPart("attachments", []string{fmt.Sprintf("%v", w.mounts.mounts)})
	if w.step == WizardStepMounts {
		form.TrackDraftPart("attachment editor", []string{
			w.mounts.source.String(),
			w.mounts.destination.String(),
			fmt.Sprintf("%v", w.mounts.access),
		})
	}
	if w.step == WizardStepProjectDirectory {
		form.TrackDraftPart("project", []string{w.projectDirectory.String()})
	}
	if w.step == WizardStepNewBundle {
		form.TrackDraftPart("repositories", []string{
			w.newBundleSource.String(),
			fmt.Sprintf("%q", w.newBundleRepositories),
		})
	}
}

func (w *ResumeWizard) prepareDialogState() {
	form := &w.form
	form.TrackDraftPart("queued work", []string{strconv.FormatBool(w.discardQueue)})
	form.SetDismissActions([]WizardControl{WizardControlCancel, WizardControlBack})
	var escape *WizardControl
	if w.step == WizardStepMounts || w.step == WizardStepNewBundle {
		back := WizardControlBack
		escape = &back
	}
	form.SetEscapeAction(escape)
	form.SetDismissalScope(WizardControlBack, "attachment editor")
	form.TrackDraftPart("attachments", []string{fmt.Sprintf("%v", w.mounts.mounts)})
	if w.step == WizardStepMounts {
		form.TrackDraftPart("attachment editor", []string{
			w.mounts.source.String(),
			w.mounts.destination.String(),
			fmt.Sprintf("%v", w.mounts.access),
		})
	}
}

func (w *ResumeWizard) hasQueuedWork(dashboard *DashboardState) bool {
	if w.preparation != nil {
		return len(w.preparation.QueuedCommands) > 0
	}
	detail, ok := (*dashboard.sessionDetails.Get())[w.sessionID]
	return ok && len(detail.queuedPrompts) > 0
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
	return wizardSubagentDraftsEqual(w.subagents, other.subagents) &&
		w.sessionID == other.sessionID &&
		w.source == other.source &&
		w.title == other.title &&
		w.workspaceID == other.workspaceID &&
		w.moving == other.moving &&
		reflect.DeepEqual(w.preparation, other.preparation) &&
		w.preparing == other.preparing &&
		wizardOptionalEqual(w.preparationRequestID, other.preparationRequestID) &&
		wizardOptionalStringsEqual(w.preflightError, other.preflightError) &&
		wizardOptionalStringsEqual(w.preparationError, other.preparationError) &&
		w.step == other.step &&
		w.profile == other.profile &&
		w.target == other.target &&
		w.profileStepSkipped == other.profileStepSkipped &&
		w.targetStepSkipped == other.targetStepSkipped &&
		w.mounts.equal(&other.mounts) &&
		w.resourceAllocation == other.resourceAllocation &&
		wizardOptionalStringsEqual(w.sizingError, other.sizingError) &&
		w.resourceEditor.Equal(&other.resourceEditor) &&
		w.discardQueue == other.discardQueue
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
	if len(mounts.mounts) == 0 {
		return false
	}
	index := mounts.selectedMount
	mounts.historySelection = 0
	mount := mounts.mounts[index]
	mounts.source = chat.PathInputFromValue(mount.Source)
	mounts.destination = chat.PathInputFromValue(mount.Destination)
	mounts.access = mount.Access
	if _, unavailable := mounts.overlayUnavailable(); unavailable {
		mounts.forbidOverlay()
	}
	mounts.error = nil
	mounts.editingMount = &index
	mounts.source.DismissCompletion()
	return true
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
	if err := core.ValidateAbsoluteInput(strings.TrimSpace(mounts.source.Value())); err != nil {
		return err.Error(), true
	}
	mount := core.AdditionalMount{
		Source:      mounts.source.String(),
		Destination: mounts.destination.String(),
		Access:      mounts.access,
	}
	if err := core.ValidateMountDestination(mount.Destination); err != nil {
		return err.Error(), true
	}
	for index, existing := range mounts.mounts {
		if (mounts.editingMount == nil || *mounts.editingMount != index) && existing.Destination == mount.Destination {
			return fmt.Sprintf("%s is already an attached directory destination.", mount.Destination), true
		}
	}
	return "", false
}

func (w *NewWizard) addNewBundleRepository() bool {
	source := strings.TrimSpace(w.newBundleSource.Value())
	if source == "" {
		return false
	}
	if !slices.Contains(w.newBundleRepositories, source) {
		w.newBundleRepositories = append(w.newBundleRepositories, source)
	}
	w.newBundleSelected = len(w.newBundleRepositories) - 1
	w.newBundleSource.Clear()
	w.form.Focus(WizardControlNewBundleSource)
	return true
}

func (w *NewWizard) removeSelectedNewBundleRepository() bool {
	if len(w.newBundleRepositories) == 0 {
		return false
	}
	index := min(w.newBundleSelected, len(w.newBundleRepositories)-1)
	w.newBundleRepositories = slices.Delete(w.newBundleRepositories, index, index+1)
	w.newBundleSelected = min(w.newBundleSelected, max(len(w.newBundleRepositories)-1, 0))
	if len(w.newBundleRepositories) == 0 {
		w.form.Focus(WizardControlNewBundleSource)
	} else {
		w.form.Focus(WizardControlNewBundleRepositories)
	}
	return true
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
	var newest *client.LaunchRecency
	var newestCreated *int64
	for index := range recency {
		launch := &recency[index]
		if _, ok := config.EnabledProfile(launch.LastProfile); !ok {
			continue
		}
		if _, ok := config.Bundles[launch.BundleID]; !ok {
			continue
		}
		if _, ok := config.Targets[launch.TargetTemplateID]; !ok {
			continue
		}
		var created *int64
		if ms, ok := createdAtMs(launch.NewestCreatedAt); ok {
			created = &ms
		}
		// Rust's max_by_key keeps the last of equal maxima; None sorts first.
		if newest == nil || wizardCompareOptionalMs(created, newestCreated) >= 0 {
			newest, newestCreated = launch, created
		}
	}
	return newest
}

// wizardCompareOptionalMs is the Ord of `Option<i64>`: None before any value.
func wizardCompareOptionalMs(left, right *int64) int {
	switch {
	case left == nil && right == nil:
		return 0
	case left == nil:
		return -1
	case right == nil:
		return 1
	}
	return cmp.Compare(*left, *right)
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
	latestCreatedAt := map[string]int64{}
	for _, launch := range recency {
		if _, ok := config.Bundles[launch.BundleID]; !ok {
			continue
		}
		created, ok := createdAtMs(launch.NewestCreatedAt)
		if !ok {
			continue
		}
		if latest, seen := latestCreatedAt[launch.BundleID]; !seen || created > latest {
			latestCreatedAt[launch.BundleID] = created
		}
	}
	bundleIDs := slices.Sorted(maps.Keys(config.Bundles))
	latest := func(id string) *int64 {
		if created, ok := latestCreatedAt[id]; ok {
			return &created
		}
		return nil
	}
	slices.SortStableFunc(bundleIDs, func(left, right string) int {
		return cmp.Or(wizardCompareOptionalMs(latest(right), latest(left)), strings.Compare(left, right))
	})
	return bundleIDs
}

func targetLabel(target core.TargetTemplate) string {
	switch target.(type) {
	case core.TargetTemplateLocalBare:
		return "localhost"
	case core.TargetTemplateLocalPodman:
		return "local Podman"
	case core.TargetTemplateLocalDocker:
		return "local Docker"
	case core.TargetTemplateAppleContainer:
		return "Apple container"
	case core.TargetTemplateAwsEc2:
		return "AWS EC2"
	case core.TargetTemplateSshBare:
		return "named SSH machine"
	case core.TargetTemplateSshPodman:
		return "Podman over SSH"
	case core.TargetTemplateSshDocker:
		return "Docker over SSH"
	case core.TargetTemplateSshAppleContainer:
		return "Apple container over SSH"
	case core.TargetTemplateSshNitro:
		return "EC2 Nitro sandbox"
	}
	panic(fmt.Sprintf("unknown target template %T", target))
}

// resourceAllocationDescription is the compute a session gets, on its own,
// as the review's Compute row shows it. A nil allocation is fixed or
// default resources.
func resourceAllocationDescription(allocation core.SessionResourceAllocation) string {
	switch allocation := allocation.(type) {
	case core.SessionResourceAllocationContainer:
		return fmt.Sprintf("%d CPU / %s", allocation.Cpus, formatResourceBytes(allocation.MemoryBytes))
	case core.SessionResourceAllocationAwsEc2:
		return fmt.Sprintf("%s · %d CPU / %s", allocation.InstanceType, allocation.Vcpus, formatResourceBytes(allocation.MemoryBytes))
	}
	return "fixed/default resources"
}

func defaultResourceDestination(target core.TargetTemplate, source string, existing []core.AdditionalMount) string {
	defaultDestination := core.DefaultMountDestination(source, existing)
	ec2, ok := target.(core.TargetTemplateAwsEc2)
	if !ok {
		return defaultDestination
	}
	basename, ok := resumePathFileName(defaultDestination)
	if !ok {
		basename = "resource"
	}
	// EC2 paths are POSIX paths on a Windows controller too.
	home := "/root"
	if ec2.SSHUser != "root" {
		home = core.Join("/home", ec2.SSHUser)
	}
	resources := core.Join(home, "mj-resources")
	taken := func(candidate string) bool {
		return slices.ContainsFunc(existing, func(resource core.AdditionalMount) bool {
			return resource.Destination == candidate
		})
	}
	base := core.Join(resources, basename)
	if !taken(base) {
		return base
	}
	for number := 2; ; number++ {
		candidate := core.Join(resources, fmt.Sprintf("%s-%d", basename, number))
		if !taken(candidate) {
			return candidate
		}
	}
}
