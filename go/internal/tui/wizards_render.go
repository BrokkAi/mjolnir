// Port of mj-tui/src/wizards/render.rs.
//
// Drawing for the new-session and resume/move wizards: their picker steps,
// project and mount steps, and the review step both share.

package tui

import (
	"fmt"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// targetIsSized says whether the target step sets a size for target:
// containers get CPU and memory, EC2 an instance type. Other targets run with
// what the host has.
func targetIsSized(target core.TargetTemplate) bool {
	if core.IsContainerTarget(target) {
		return true
	}
	_, ec2 := target.(core.TargetTemplateAwsEc2)
	return ec2
}

// targetStepChoices is the target step's table and the help lines under it,
// shared by New session and Resume/Move so their rows match, with the
// selected row. A row carries a short status; the full reason a target is
// unavailable goes under the table, where it wraps instead of being cut off.
// sizingError is nil when sizing succeeded.
func targetStepChoices[W WizardDraft](dashboard *DashboardState, wizard W, sizingError *string) ([]PickerChoice, []chat.Line, int) {
	panic("unported: mj-tui/src/wizards/render.rs target_step_choices")
}

// missingProfileHelp is the lines under a profile list naming each enabled
// profile it leaves out and why.
func missingProfileHelp(dashboard *DashboardState) []chat.Line {
	warning := chat.Style{}.Foreground(chat.ThemePalette().Warning)
	missing := dashboard.missingProfileLines()
	lines := make([]chat.Line, 0, len(missing))
	for _, line := range missing {
		lines = append(lines, chat.StyledLine(line, warning))
	}
	return lines
}

func newWizardProfileRows(dashboard *DashboardState, wizard *NewWizard) []PickerChoice {
	panic("unported: mj-tui/src/wizards/render.rs new_wizard_profile_rows")
}

func stepInitial(step WizardStep) WizardControl {
	switch step {
	case WizardStepProfile:
		return WizardControlProfileList
	case WizardStepTarget:
		return WizardControlTargetList
	case WizardStepBundle:
		return WizardControlBundleList
	case WizardStepProjectDirectory:
		return WizardControlProjectDirectory
	case WizardStepNewBundle:
		return WizardControlProjectResults
	case WizardStepMounts:
		return WizardControlMountSource
	default:
		// Review and Launching.
		return WizardControlSubmit
	}
}

// launchDirectoryChosen says whether the wizard's project is the repository
// `mj` was started in, which the project step fills in without being asked.
func launchDirectoryChosen(dashboard *DashboardState, wizard *NewWizard) bool {
	panic("unported: mj-tui/src/wizards/render.rs launch_directory_chosen")
}

// targetStepHidden says whether the wizard's title leaves the target step out
// of its step numbers. On the profile step this is a forecast of whether
// Next there would pass the target step.
func targetStepHidden[W WizardDraft](dashboard *DashboardState, wizard W) bool {
	panic("unported: mj-tui/src/wizards/render.rs target_step_hidden")
}

// stepCounter is a step's place in its wizard's title, such as `2/4`.
// position and total count every step, with the target step second; a
// hidden target step is taken out of both.
func stepCounter[W WizardDraft](position int, total int, targetHidden bool, wizard W) string {
	// count is Rust's usize::from(bool).
	count := func(value bool) int {
		if value {
			return 1
		}
		return 0
	}
	profileHidden := wizard.ProfileStepSkipped()
	reviewHidden := profileHidden && targetHidden
	skipped := count(profileHidden) + count(targetHidden) + count(reviewHidden)
	before := count(profileHidden && position > 1) + count(targetHidden && position > 2)
	return fmt.Sprintf("%d/%d", position-before, total-skipped)
}

// renderLaunching draws the step a submitted wizard waits on; err is the
// failure that stopped the launch, nil while prerequisites are checked.
func renderLaunching(frame *chat.Frame, area uv.Rectangle, form *chat.Dialog[WizardControl], surfaces *chat.FrameSurfaces, title string, err *string) {
	panic("unported: mj-tui/src/wizards/render.rs render_launching")
}

func beginFormFrame(form *chat.Dialog[WizardControl], initial WizardControl) {
	form.BeginFrame()
}

func renderNewWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, wizard *NewWizard, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/wizards/render.rs render_new_wizard")
}

// repositorySource is where a project repository comes from, as the user
// gave it: the GitHub name, else the local path with the home directory
// written as `~`.
func repositorySource(repository *core.ProjectRepository) string {
	panic("unported: mj-tui/src/wizards/render.rs repository_source")
}

// projectSourceChars is the longest source a project row shows before it
// cuts the middle out.
const projectSourceChars = 40

// compactPath shortens a long source by cutting its middle: the start says
// where it lives and the end says which one it is. The details under the
// list show it whole.
func compactPath(source string) string {
	chars := []rune(source)
	if len(chars) <= projectSourceChars {
		return source
	}
	head := projectSourceChars / 3
	tail := projectSourceChars - head - 1
	return string(chars[:head]) + "…" + string(chars[len(chars)-tail:])
}

// bundleDetails is the selected project's full sources, drawn under the
// project list so a long path or a second repository is never cut off.
// missing lists the project's local directories that do not exist.
func bundleDetails(id string, bundle *core.ProjectBundle, missing []string) []chat.Line {
	panic("unported: mj-tui/src/wizards/render.rs bundle_details")
}

// reviewWizardWorktree is Rust's `(bool, bool)` worktree choice of the review
// step: whether an isolated checkout is chosen, and whether one can be made.
type reviewWizardWorktree struct {
	create    bool
	available bool
}

// reviewWizardQueue is Rust's `(usize, bool)` queue line of the review step:
// how many commands are queued, and whether they are discarded.
type reviewWizardQueue struct {
	count   int
	discard bool
}

// ReviewWizardView is what the review step shows, shared by New session and
// Resume/Move. Rust's borrowed Option fields are nil pointers here.
type ReviewWizardView struct {
	// worktree is the isolated-checkout choice; nil when the target always
	// provides the workspace.
	worktree     *reviewWizardWorktree
	profileID    string
	projectLabel string
	project      string
	projectNote  string
	targetID     string
	// allocation is the chosen size; nil for none.
	allocation         core.SessionResourceAllocation
	mounts             *MountWizard
	title              string
	submitLabel        string
	moving             bool
	preparing          bool
	preparationError   *string
	submitEnabled      bool
	activeInterruption bool
	// inPlaceMove is true when the prepared move keeps the environment and
	// replaces only the harness and profile, so the review must not promise a
	// fresh environment.
	inPlaceMove              bool
	destinationChecksPending bool
	sourceUnavailable        bool
	// stoppedSubagents is the sub-agents the Move stops that had not handed
	// back; idle ones are stopped without a word, as a suspend stops them.
	stoppedSubagents        int
	subagents               *SubagentDraft
	clearResourceAllocation bool
	queue                   *reviewWizardQueue
	queuedEntries           []core.QueuedPrompt
	preparedEntries         []core.MaterializedQueuedPrompt
	// remoteRepositories is the resolved network plan; nil until resolved.
	remoteRepositories      *[]RemoteRepositoryPreview
	remotePreflightInFlight bool
	remotePreflightError    *string
	localChangesExcluded    bool
	// reviewNotice is why creating this session turns its review off, when
	// it will.
	reviewNotice *string
	// conversion is present only when a move converts a local checkout into
	// an isolated workspace, so the review can say what travels before the
	// confirmation.
	conversion   *core.RawConversionPreview
	sandboxOffer *SandboxOffer
}

func renderReviewWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, view ReviewWizardView, form *chat.Dialog[WizardControl], surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/wizards/render.rs render_review_wizard")
}

// accessMarker is the suffix that shows an attached directory's access mode
// in a list row.
func accessMarker(access core.MountAccess) string {
	return " · " + access.Label()
}

// accessChoices is the access modes offered for an attachment, as the shared
// rule in core.MountAccessOffered defines them.
func accessChoices(overlayUnavailable bool) []core.MountAccess {
	return core.MountAccessOffered(!overlayUnavailable)
}

func accessDescription(access core.MountAccess) string {
	switch access {
	case core.MountAccessCow:
		return "cow · container-private copy-on-write"
	case core.MountAccessRw:
		return "rw · writes reach the host directory"
	default:
		return "ro · read-only"
	}
}

// accessComboKind is the form control kind for an access-mode combobox.
func accessComboKind[K comparable](combo *chat.ComboBoxState[K], choices []core.MountAccess, access core.MountAccess, id K) chat.ControlKind {
	return chat.ControlKindComboBox{
		Len:      len(choices),
		Selected: combo.Selection(id, accessIndex(choices, access)),
		Expanded: combo.IsOpen(id),
	}
}

func accessIndex(choices []core.MountAccess, access core.MountAccess) int {
	for index, choice := range choices {
		if choice == access {
			return index
		}
	}
	return 0
}

// renderAccessCombo draws the access-mode combobox for an attachment under
// edit. Screens call this once in place and, while it is open, again after
// everything else so the popup stays on top.
func renderAccessCombo[K comparable](frame *chat.Frame, bounds uv.Rectangle, field uv.Rectangle, access core.MountAccess, choices []core.MountAccess, combo *chat.ComboBoxState[K], expanded bool, form *chat.Form[K], id K) {
	selected := combo.Selection(id, accessIndex(choices, access))
	// ComboBoxRender adds the dropdown glyph itself.
	value := ""
	if selected >= 0 && selected < len(choices) {
		value = accessDescription(choices[selected])
	}
	options := make([]chat.Line, 0, len(choices))
	for _, choice := range choices {
		options = append(options, chat.RawLine(accessDescription(choice)))
	}
	chat.ComboBoxRender(frame, bounds, field, value, options, selected, expanded, true, " Access ",
		chat.PopupSideBelow, chat.ComboBoxDefaultMaxVisibleRows, form, id)
}

// renderMountWizard draws the attached-directories step. Domain mount data,
// navigation, and the shared form are separate inputs.
func renderMountWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, targetIndex int, mounts *MountWizard, form *chat.Dialog[WizardControl], title string, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/wizards/render.rs render_mount_wizard")
}

// resumeWizardTitle is the title bar of one step of the resume wizard.
//
// The same three steps start a resumed session, a moved one, and a session
// restored from an archived transcript, so the first word says which.
func resumeWizardTitle(wizard *ResumeWizard, step string, movingStep string) string {
	switch {
	case wizard.moving:
		return fmt.Sprintf(" Move · %s ", movingStep)
	case wizard.source == ResumeSourceArchive:
		return fmt.Sprintf(" Restore · %s ", step)
	default:
		return fmt.Sprintf(" Resume · %s ", step)
	}
}

func renderResumeWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, wizard *ResumeWizard, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/wizards/render.rs render_resume_wizard")
}
