// Port of mj-tui/src/wizards/render.rs.
//
// Drawing for the new-session and resume/move wizards: their picker steps,
// project and mount steps, and the review step both share.

package tui

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	warning := chat.Style{}.Foreground(chat.ThemePalette().Warning)
	var help []chat.Line
	// Only what is there is listed; what is not is named under the table.
	offered := dashboard.offeredTargetIndices()
	selectedRow := dashboard.targetRow(wizard.Target())
	config := dashboard.config.Get()
	var rows []PickerChoice
	for index, id := range wizardsSortedTargetIDs(config) {
		if !slices.Contains(offered, index) {
			continue
		}
		target := config.Targets[id]
		selected := index == wizard.Target()
		rejection, rejected := wizard.TargetRejection(dashboard, id)
		cpu, memory := "—", "—"
		if allocation := wizard.ResourceAllocation(); allocation != nil && selected && targetIsSized(target) {
			cpu = strconv.FormatUint(core.AllocationCpus(allocation), 10)
			memory = memoryGibText(core.AllocationMemory(allocation))
		}
		status := pickerCellBlank()
		switch {
		case rejected:
			status = pickerCellStyled("unavailable", warning)
		case selected && sizingError != nil:
			status = pickerCellStyled("sizing failed", warning)
		}
		// A row this draft cannot use cannot be selected, so each one's
		// reason is listed under the table rather than only the selected
		// row's.
		if rejected {
			help = append(help, chat.StyledLine(id+": "+rejection, warning))
		}
		if selected {
			if core.IsContainerTarget(target) {
				if capacity := dashboard.hostContainerCapacity(id); capacity != nil {
					remaining := "RAM left unavailable (live container sizing unknown)"
					if capacity.Complete {
						remaining = fmt.Sprintf(
							"%d CPUs · %s GiB RAM left",
							capacity.Remaining.Cpus,
							capacityGibText(capacity.Remaining.MemoryBytes),
						)
					}
					help = append(help, chat.StyledLine(
						fmt.Sprintf(
							"Host: %d CPUs · %s GiB RAM · committed; %s",
							capacity.Total.Cpus,
							capacityGibText(capacity.Total.MemoryBytes),
							remaining,
						),
						chat.Muted(),
					))
					// The Host line already says when the RAM left is unknown.
					if capacity.Complete {
						if allocation := wizard.ResourceAllocation(); allocation != nil {
							if message, ok := capacity.OvercommitWarning(allocation); ok {
								help = append(help, chat.StyledLine(message, warning))
							}
						}
					}
				} else if dashboard.hostLimits(id) == nil {
					help = append(help, chat.StyledLine(
						"Host totals unavailable; resource limits cannot be checked.",
						warning,
					))
				}
			}
			if sizingError != nil && !rejected {
				help = append(help, chat.StyledLine("Sizing: "+*sizingError, warning))
			}
		}
		row := pickerChoiceTable([]PickerCell{
			pickerCellBlank(),
			pickerCellText(id),
			pickerCellText(targetLabel(target)),
			pickerCellText(cpu),
			pickerCellText(memory),
			status,
		})
		if rejected {
			row = row.intoDisabled()
		}
		rows = append(rows, row)
	}
	// What is not there has no row, so it is named here with the reason,
	// rather than missing from the list without a word.
	for _, line := range dashboard.missingTargetLines() {
		style := warning
		if line.Routine {
			style = chat.Muted()
		}
		help = append(help, chat.StyledLine(line.Text, style))
	}
	return targetTable(rows), help, selectedRow
}

// wizardsSortedTargetIDs is the configured target ids in BTreeMap order.
func wizardsSortedTargetIDs(config *core.Config) []string {
	ids := make([]string, 0, len(config.Targets))
	for id := range config.Targets {
		ids = append(ids, id)
	}
	slices.Sort(ids)
	return ids
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
	expandedProviders := wizard.effectiveExpandedProviders(dashboard)
	items := newWizardProfileItems(dashboard, expandedProviders)
	predictedCells := func(id string, predicted bool) (PickerCell, PickerCell, PickerCell) {
		if !predicted {
			return pickerCellStyled(chat.ThemeGlyphs().None, chat.Muted()), pickerCellBlank(), pickerCellBlank()
		}
		weekly, fiveHour := dashboard.profileQuotaCells(id)
		return pickerCellText(id), weekly, fiveHour
	}
	var rows []PickerChoice
	for _, item := range items {
		switch item := item.(type) {
		case ProfilePickerItemAuto:
			profileCell, weekly, fiveHour := predictedCells(dashboard.predictedAutoPrimaryProfile())
			rows = append(rows, pickerChoiceTable([]PickerCell{
				pickerCellBlank(),
				pickerCellText("Auto"),
				profileCell,
				weekly,
				fiveHour,
			}))
		case ProfilePickerItemProvider:
			_, expanded := expandedProviders[item.Provider]
			arrow := "▸"
			switch {
			case expanded:
				arrow = chat.ThemeGlyphs().Dropdown
			case chat.ASCII():
				arrow = ">"
			}
			headingStyle := chat.Style{}.Foreground(providerAccent(item.Provider)).Bold()
			profileCell, weekly, fiveHour := predictedCells(dashboard.predictedProviderProfile(item.Provider))
			rows = append(rows, pickerChoiceTable([]PickerCell{
				pickerCellBlank(),
				pickerCellStyled(arrow+" "+strings.ToUpper(item.Provider.DisplayName()), headingStyle),
				profileCell,
				weekly,
				fiveHour,
			}))
		case ProfilePickerItemProfile:
			rows = append(rows, dashboard.newWizardProfileChoice(item.ID))
		}
	}
	return newWizardProfileTable(rows)
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
	launch := dashboard.launchProjectDirectory
	if launch == nil {
		return false
	}
	// Rust compares Paths, component by component.
	return filepath.Clean(strings.TrimSpace(wizard.projectDirectory.Value())) == filepath.Clean(*launch)
}

// targetStepHidden says whether the wizard's title leaves the target step out
// of its step numbers. On the profile step this is a forecast of whether
// Next there would pass the target step.
func targetStepHidden[W WizardDraft](dashboard *DashboardState, wizard W) bool {
	if wizard.Step() == WizardStepProfile {
		_, lone := dashboard.loneTarget(wizard)
		return lone
	}
	return wizard.TargetStepSkipped()
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
	popup := wizardModal(frame, surfaces, area)
	text := "Checking prerequisites…"
	if err != nil {
		text = *err
	}
	lines := chat.WrapLines([]chat.Line{chat.RawLine(text)}, max(popup.Dx()-4, 0))
	inner := chat.DialogShellPaddedInner(popup)
	titleLine := dismissibleModalTitle(&form.Form, popup, title, chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	layout := chat.DialogShellLayout(inner, 1)
	frame.RenderWidget(dialogsParagraph(lines), layout.Body)
	buttons := []chat.ButtonSpec[WizardControl]{{ID: WizardControlCancel, Label: "Cancel", Enabled: true}}
	if err != nil {
		buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlSubmit, Label: "Retry", Enabled: true})
	}
	form.RenderActions(frame, layout.Actions, buttons)
}

func beginFormFrame(form *chat.Dialog[WizardControl], initial WizardControl) {
	form.BeginFrame()
}

// wizardsLaunchError is a wizard's launch error as an optional string.
func wizardsLaunchError(message string, ok bool) *string {
	if !ok {
		return nil
	}
	return &message
}

// wizardsRecentProjectRow is one recent directory drawn on the project step: its
// index in the history, its first content row and how many rows it wraps to.
type wizardsRecentProjectRow struct {
	index, start, height int
}

func renderNewWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, wizard *NewWizard, surfaces *chat.FrameSurfaces) {
	form := &wizard.form
	// Read focus before the frame starts: BeginFrame hides last frame's
	// registrations, so IsFocused is false for a control drawn before it
	// registers again, as the recent-directory rows are.
	focusedBeforeFrame, hasFocusBeforeFrame := form.Focused()
	initial := stepInitial(wizard.step)
	beginFormFrame(form, initial)
	targetHidden := targetStepHidden(dashboard, wizard)
	config := dashboard.config.Get()
	if wizard.step == WizardStepLaunching {
		renderLaunching(frame, area, form, surfaces, "Creating session", wizardsLaunchError(wizard.LaunchError()))
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepReview {
		targetID := nthKey(config.Targets, wizard.target)
		rawProject := core.IsBareProjectTarget(config.Targets[targetID])
		title := fmt.Sprintf(" New session · %s review ", stepCounter(4, 4, targetHidden, wizard))
		bundleID := ""
		if !rawProject {
			bundleID = nthBundleKey(config, dashboard.launchRecency, wizard.bundle)
		}
		options, hasOptions := wizard.selectedWorktreeOptions(config)
		// Isolated targets always provide the workspace, so the choice only
		// exists for a bare project directory.
		var worktree *reviewWizardWorktree
		if rawProject {
			worktree = &reviewWizardWorktree{
				create:    wizard.createManagedWorktree,
				available: hasOptions && options.Available,
			}
		}
		projectLabel, project, projectNote := "Project", bundleID, ""
		if rawProject {
			projectLabel = "Project directory"
			project = strings.TrimSpace(wizard.projectDirectory.Value())
			if launchDirectoryChosen(dashboard, wizard) {
				projectNote = "  (the repository mj was started in)"
			}
		}
		submitLabel := "Create"
		if wizard.remotePreflightError != nil {
			submitLabel = "Retry"
		}
		_, subagentError := wizard.subagents.error()
		var reviewNotice *string
		if rawProject && wizard.selectedReviewTurnsOff(config) {
			notice := core.PlainDirectoryReviewOff
			reviewNotice = &notice
		}
		renderReviewWizard(
			frame,
			area,
			dashboard,
			ReviewWizardView{
				worktree:     worktree,
				profileID:    wizard.selectedProfileLabel(dashboard),
				projectLabel: projectLabel,
				project:      project,
				projectNote:  projectNote,
				targetID:     targetID,
				allocation:   wizard.resourceAllocation,
				mounts:       &wizard.mounts,
				title:        title,
				submitLabel:  submitLabel,
				submitEnabled: (!rawProject || hasOptions || wizard.remotePreflightError != nil) &&
					!subagentError,
				subagents:               wizard.subagents,
				remoteRepositories:      wizard.remoteRepositories,
				remotePreflightInFlight: wizard.remotePreflightInFlight,
				remotePreflightError:    wizard.remotePreflightError,
				localChangesExcluded:    !rawProject,
				reviewNotice:            reviewNotice,
				sandboxOffer:            dashboard.sandboxOffer(wizard),
			},
			form,
			surfaces,
		)
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepProjectDirectory {
		renderNewWizardProjectDirectory(frame, area, dashboard, wizard, surfaces, focusedBeforeFrame, hasFocusBeforeFrame, targetHidden)
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepMounts {
		renderMountWizard(frame, area, dashboard, wizard.target, &wizard.mounts, form, " Add attached directory ", surfaces)
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepNewBundle {
		renderProjectPicker(frame, area, wizard, form, surfaces)
		form.EndFrame(initial)
		return
	}
	var stepHelp []chat.Line
	var title string
	var choices []PickerChoice
	var selected int
	var control WizardControl
	switch wizard.step {
	case WizardStepProfile:
		title = fmt.Sprintf(" New session · %s profile ", stepCounter(1, 4, targetHidden, wizard))
		choices = newWizardProfileRows(dashboard, wizard)
		selected = wizard.ProfileListSelected(dashboard)
		control = WizardControlProfileList
	case WizardStepBundle:
		title = fmt.Sprintf(" New session · %s choose a project ", stepCounter(3, 4, targetHidden, wizard))
		ids := bundleIDsByRecentCreation(config, dashboard.launchRecency)
		if wizard.bundle >= 0 && wizard.bundle < len(ids) {
			id := ids[wizard.bundle]
			bundle := config.Bundles[id]
			stepHelp = bundleDetails(id, &bundle, dashboard.missingProjectDirectories(&bundle))
		}
		for _, id := range ids {
			bundle := config.Bundles[id]
			summary := ""
			if len(bundle.Repositories) > 0 {
				summary = compactPath(repositorySource(&bundle.Repositories[0]))
			}
			if len(bundle.Repositories) > 1 {
				summary += fmt.Sprintf("  +%d more", len(bundle.Repositories)-1)
			}
			if len(dashboard.missingProjectDirectories(&bundle)) > 0 {
				summary += "  (unavailable)"
			}
			choices = append(choices, pickerChoiceTable([]PickerCell{
				pickerCellText(id),
				pickerCellStyled(summary, chat.Muted()),
			}))
		}
		selected = wizard.bundle
		control = WizardControlBundleList
	case WizardStepTarget:
		var rows []PickerChoice
		rows, stepHelp, selected = targetStepChoices(dashboard, wizard, wizard.sizingError)
		title = fmt.Sprintf(" New session · %s target ", stepCounter(2, 4, targetHidden, wizard))
		choices = rows
		control = WizardControlTargetList
	default:
		panic("the review, mount, bundle and project directory steps were rendered above")
	}
	var help []chat.Line
	if wizard.step == WizardStepTarget || wizard.step == WizardStepBundle {
		help = stepHelp
	}
	if wizard.step == WizardStepBundle {
		var feedback *string
		if wizard.bundleRemovalInFlight {
			removing := "Removing project…"
			feedback = &removing
		} else {
			feedback = wizard.bundleRemovalOutcome
		}
		if feedback != nil {
			// Put the outcome before repository details so a long project
			// cannot push the reason for a refused removal out of view.
			help = append([]chat.Line{
				chat.StyledLine(*feedback, chat.Style{}.Foreground(chat.ThemePalette().Warning)),
				chat.RawLine(""),
			}, help...)
		}
	}
	if wizard.step == WizardStepProfile {
		help = append(help, missingProfileHelp(dashboard)...)
	}
	nextEnabled := true
	var sideActions []chat.ButtonSpec[WizardControl]
	var emptyHint *string
	switch wizard.step {
	case WizardStepProfile:
		nextEnabled = wizard.ProfileAdvanceEnabled(dashboard)
	case WizardStepTarget:
		nextEnabled = targetAdvanceEnabled(dashboard, wizard)
	case WizardStepBundle:
		// Without a bundle there is nothing to review; the pinned action is
		// the only way forward.
		nextEnabled = len(config.Bundles) > 0
		sideActions = []chat.ButtonSpec[WizardControl]{
			{ID: WizardControlAdd, Label: "Add…", Enabled: true},
			{ID: WizardControlRemoveBundle, Label: "Remove", Enabled: len(config.Bundles) > 0},
		}
		if len(config.Bundles) == 0 {
			hint := "Choose a project to get started."
			emptyHint = &hint
		}
	}
	renderPicker(
		frame,
		area,
		title,
		choices,
		help,
		PickerNavigation{
			resources:   targetResources(dashboard, wizard),
			hasBack:     wizard.HasBack(),
			selected:    selected,
			control:     control,
			nextEnabled: nextEnabled,
			sideActions: sideActions,
			emptyHint:   emptyHint,
		},
		form,
		surfaces,
	)
	form.EndFrame(control)
}

// renderNewWizardProjectDirectory draws the new-session wizard's project
// directory step (the ProjectDirectory arm of Rust's render_new_wizard).
func renderNewWizardProjectDirectory(
	frame *chat.Frame,
	area uv.Rectangle,
	dashboard *DashboardState,
	wizard *NewWizard,
	surfaces *chat.FrameSurfaces,
	focusedBeforeFrame WizardControl,
	hasFocusBeforeFrame bool,
	targetHidden bool,
) {
	form := &wizard.form
	config := dashboard.config.Get()
	targetID := nthKey(config.Targets, wizard.target)
	_, local := config.Targets[targetID].(core.TargetTemplateLocalBare)
	popup := wizardModal(frame, surfaces, area)
	width := max(popup.Dx()-4, 0)
	introText := "Absolute project directory on the remote machine:"
	if local {
		introText = "Absolute project directory on this machine:"
	}
	intro := chat.WrapLines([]chat.Line{chat.RawLine(introText)}, width)
	fieldRow := len(intro) + 1
	details := []chat.Line{chat.RawLine("")}

	if wizard.projectDirectoryError != nil {
		details = append(details, chat.WrapLines([]chat.Line{chat.StyledLine(
			"Error: "+*wizard.projectDirectoryError,
			chat.Style{}.Foreground(chat.ThemePalette().Error),
		)}, width)...)
		details = append(details, chat.RawLine(""))
	}
	if local && launchDirectoryChosen(dashboard, wizard) {
		details = append(details, chat.WrapLines([]chat.Line{chat.StyledLine(
			"Filled in with the repository mj was started in.",
			chat.Style{}.Foreground(chat.ThemePalette().Warning),
		)}, width)...)
		details = append(details, chat.RawLine(""))
	}
	var recentRows []wizardsRecentProjectRow
	if len(wizard.projectHistory) > 0 {
		details = append(details, chat.StyledLine("Recent on this host:", chat.Muted()), chat.RawLine(""))
		for index, directory := range wizard.projectHistory[:min(len(wizard.projectHistory), 5)] {
			start := fieldRow + 1 + len(details)
			marker := " "
			if index == wizard.projectHistoryIndex {
				marker = "›"
			}
			style := chat.Muted()
			switch {
			case hasFocusBeforeFrame && focusedBeforeFrame == WizardControlRecentProject(index):
				style = chat.Selection(true)
			case index == wizard.projectHistoryIndex:
				style = chat.Style{}.Foreground(chat.ThemePalette().Text)
			}
			rows := chat.WrapLines([]chat.Line{chat.StyledLine(marker+" "+directory, style)}, width)
			recentRows = append(recentRows, wizardsRecentProjectRow{index: index, start: start, height: len(rows)})
			details = append(details, rows...)
		}
	}
	for len(details) > 0 && details[len(details)-1].Width() == 0 {
		details = details[:len(details)-1]
	}
	height := fieldRow + 1 + max(len(details), chat.PathFieldPopupRows(&wizard.projectDirectory))
	inner := chat.DialogShellPaddedInner(popup)
	layout := chat.DialogShellLayout(inner, 1)
	focusedRow := &chat.RowRange{Start: fieldRow, End: fieldRow + 1}
	if hasFocusBeforeFrame {
		if index, recent := focusedBeforeFrame.RecentProjectIndex(); recent {
			focusedRow = nil
			for _, row := range recentRows {
				if row.index == index {
					focusedRow = &chat.RowRange{Start: row.start, End: row.start + row.height}
					break
				}
			}
		}
	}
	viewport := form.Viewport(layout.Body, height, focusedRow)
	locality := "remote"
	if local {
		locality = "local"
	}
	title := dismissibleModalTitle(
		&form.Form,
		popup,
		fmt.Sprintf("New session · %s %s project", stepCounter(3, 4, targetHidden, wizard), locality),
		chat.Title(true),
		true,
	)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	for index, line := range intro {
		frame.RenderWidget(dialogsParagraph([]chat.Line{line}), viewport.Row(index, 1))
	}
	for index, line := range details {
		frame.RenderWidget(dialogsParagraph([]chat.Line{line}), viewport.Row(fieldRow+1+index, 1))
	}
	for _, row := range recentRows {
		form.Register(WizardControlRecentProject(row.index), chat.ControlKindButton{}, viewport.Row(row.start, row.height), true)
	}
	chat.PathFieldRenderWithin(
		frame,
		layout.Body,
		viewport.Row(fieldRow, 1),
		&wizard.projectDirectory,
		&form.Form,
		WizardControlProjectDirectory,
	)
	form.SetFieldUsesVerticalKeys(WizardControlProjectDirectory, len(wizard.projectHistory) > 0)
	buttons := []chat.ButtonSpec[WizardControl]{{ID: WizardControlCancel, Label: "Cancel", Enabled: true}}
	if wizard.HasBack() {
		buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlBack, Label: "Back", Enabled: true})
	}
	next := "Next"
	if wizard.SkipsReview() {
		next = "Create"
	}
	buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlNext, Label: next, Enabled: true})
	form.RenderActions(frame, layout.Actions, buttons)
}

// repositorySource is where a project repository comes from, as the user
// gave it: the GitHub name, else the local path with the home directory
// written as `~`.
func repositorySource(repository *core.ProjectRepository) string {
	if repository.Github != nil {
		return *repository.Github
	}
	if repository.Local == nil {
		return repository.ID
	}
	path := *repository.Local
	if home, ok := core.UserHome(); ok {
		if rest, ok := wizardsStripPathPrefix(path, home); ok {
			if rest == "" {
				return "~"
			}
			return "~/" + rest
		}
	}
	return path
}

// wizardsPathComponents is path split into its components the way Rust's
// Path::components reads it: a leading separator is the root, repeated
// separators and `.` parts after the first are dropped.
func wizardsPathComponents(path string) []string {
	var components []string
	if strings.HasPrefix(path, "/") || strings.HasPrefix(path, string(filepath.Separator)) {
		components = append(components, "/")
	}
	parts := strings.FieldsFunc(path, func(character rune) bool {
		return character == '/' || character == filepath.Separator
	})
	for index, part := range parts {
		if part == "." && (index > 0 || len(components) > 0) {
			continue
		}
		components = append(components, part)
	}
	return components
}

// wizardsStripPathPrefix is Rust's Path::strip_prefix: the rest of path after
// the components of base, false when base is not a component prefix of path.
func wizardsStripPathPrefix(path, base string) (string, bool) {
	pathComponents, baseComponents := wizardsPathComponents(path), wizardsPathComponents(base)
	if len(baseComponents) > len(pathComponents) {
		return "", false
	}
	for index, component := range baseComponents {
		if pathComponents[index] != component {
			return "", false
		}
	}
	return filepath.Join(pathComponents[len(baseComponents):]...), true
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
	lines := []chat.Line{chat.StyledLine(
		fmt.Sprintf("%s · %s", id, counted(len(bundle.Repositories), "repository", "repositories")),
		chat.Muted(),
	)}
	multiple := len(bundle.Repositories) > 1
	for index := range bundle.Repositories {
		repository := &bundle.Repositories[index]
		lines = append(lines, chat.RawLine(""))
		primary := ""
		if multiple && repository.ID == bundle.PrimaryRepo {
			primary = "  (primary)"
		}
		lines = append(lines, chat.RawLine("  "+repositorySource(repository)+primary))
		if repository.Local != nil && slices.Contains(missing, *repository.Local) {
			lines = append(lines, chat.StyledLine(
				fmt.Sprintf("  Unavailable: the directory %s does not exist.", *repository.Local),
				chat.Style{}.Foreground(chat.ThemePalette().Error),
			))
		}
	}
	return lines
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

// wizardsReviewCombo is a sub-agent selector drawn again on top after the rest of
// the review, because its popup is open.
type wizardsReviewCombo struct {
	id       WizardControl
	field    uv.Rectangle
	value    string
	options  []chat.Line
	selected int
	enabled  bool
}

// wizardsContentRaw is a materialized entry's content as the raw JSON values
// core.MaterializedContentText reads.
func wizardsContentRaw(content []any) []json.RawMessage {
	raw := make([]json.RawMessage, 0, len(content))
	for _, value := range content {
		encoded, err := json.Marshal(value)
		if err != nil {
			encoded = []byte("null")
		}
		raw = append(raw, encoded)
	}
	return raw
}

func renderReviewWizard(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, view ReviewWizardView, form *chat.Dialog[WizardControl], surfaces *chat.FrameSurfaces) {
	config := dashboard.config.Get()
	target := config.Targets[view.targetID]
	canAttach := core.OffersAttachedDirectories(target)
	muted := chat.Muted()
	warning := chat.Style{}.Foreground(chat.ThemePalette().Warning)
	errorStyle := chat.Style{}.Foreground(chat.ThemePalette().Error)
	mutedForeground := chat.Style{}.Foreground(chat.ThemePalette().Muted)
	mounts := view.mounts
	moving := view.moving
	blank := chat.RawLine("")
	lines := []chat.Line{
		chat.LineFrom(
			chat.StyledSpan("Profile: ", muted),
			chat.StyledSpan(view.profileID, chat.Style{}.Foreground(chat.ThemePalette().Secondary).Bold()),
		),
		chat.LineFrom(
			chat.StyledSpan(view.projectLabel+": ", muted),
			chat.StyledSpan(view.project, chat.Style{}.Foreground(chat.ThemePalette().Text).Bold()),
			chat.StyledSpan(view.projectNote, muted),
		),
		chat.LineFrom(
			chat.StyledSpan("Target: ", muted),
			chat.StyledSpan(view.targetID, chat.Style{}.Foreground(chat.ThemePalette().Accent)),
			chat.StyledSpan(fmt.Sprintf(" (%s)", targetLabel(target)), muted),
		),
		chat.LineFrom(
			chat.StyledSpan("Compute: ", muted),
			chat.RawSpan(resourceAllocationDescription(view.allocation)),
		),
	}
	if moving && view.sourceUnavailable {
		lines = append(lines, blank, chat.StyledLine(
			"Source is unavailable; Move will recover its saved data without starting its old harness.",
			warning,
		))
	}
	if moving && view.destinationChecksPending {
		lines = append(lines, blank, chat.StyledLine(core.EC2MovePreparationNotice, muted))
	}
	if moving && view.inPlaceMove {
		lines = append(lines, blank, chat.StyledLine(
			"Only the harness and profile are replaced; the environment and workspace are kept.",
			muted,
		))
	}
	if moving && view.stoppedSubagents > 0 {
		lines = append(lines, blank, chat.StyledLine(
			fmt.Sprintf(
				"%s will be stopped; the session is told which when it resumes.",
				counted(view.stoppedSubagents, "working sub-agent", "working sub-agents"),
			),
			warning,
		))
	}
	if moving && view.activeInterruption {
		interruption := "Active work will be interrupted; the session is restored into a fresh environment."
		if view.inPlaceMove {
			interruption = "Active work will be interrupted; the session keeps its environment."
		}
		lines = append(lines, blank, chat.StyledLine(interruption, warning))
		if view.clearResourceAllocation {
			lines = append(lines, blank, chat.StyledLine(
				"Fixed/default destination resources will replace the source sizing.",
				warning,
			))
		}
	}
	if conversion := view.conversion; conversion != nil && moving {
		lines = append(lines, blank, chat.RawLine(conversion.SummaryLine()))
		for _, line := range conversion.WarningLines() {
			lines = append(lines, blank, chat.StyledLine(line, warning))
		}
	}
	if moving {
		if view.preparing {
			lines = append(lines, blank, chat.StyledLine("Checking move destination…", mutedForeground))
		} else if view.preparationError != nil {
			lines = append(lines,
				blank,
				chat.StyledLine("Move preparation failed: "+*view.preparationError, errorStyle),
				blank,
				chat.StyledLine("Press Retry to check the destination again.", mutedForeground),
			)
		}
	}
	switch {
	case view.remotePreflightInFlight:
		lines = append(lines, blank, chat.StyledLine("Checking prerequisites…", mutedForeground))
	case view.remotePreflightError != nil:
		lines = append(lines, blank, chat.StyledLine("Prerequisite check failed: "+*view.remotePreflightError, errorStyle))
	case view.remoteRepositories != nil:
		plan := "Network clone plan:"
		if view.localChangesExcluded {
			plan = "Network clone plan (local commits and dirty files excluded):"
		}
		lines = append(lines, blank, chat.StyledLine(plan, muted))
		for _, repository := range *view.remoteRepositories {
			pushes := "none"
			if len(repository.PushURLs) > 0 {
				pushes = strings.Join(repository.PushURLs, ", ")
			}
			lines = append(lines, chat.RawLine(fmt.Sprintf(
				"  %s: fetch %s @ %s; push %s",
				repository.RepositoryID, repository.FetchURL, repository.DefaultBranch, pushes,
			)))
		}
	}
	// Rows of the controls drawn over the summary; -1 is none.
	worktreeRow := -1
	if worktree := view.worktree; worktree != nil {
		lines = append(lines, blank)
		worktreeRow = len(lines)
		lines = append(lines, blank)
		explanation := "Use the selected directory directly."
		if worktree.create && worktree.available {
			explanation = "Create a separate session-owned clone on the selected or default branch."
		}
		lines = append(lines, chat.StyledLine(explanation, muted))
	}
	if view.reviewNotice != nil {
		lines = append(lines, blank, chat.StyledLine(*view.reviewNotice, warning))
	}
	if view.queue != nil {
		lines = append(lines, blank, chat.RawLine(fmt.Sprintf("Queued prompts: %d", view.queue.count)))
	}
	sandboxRow := -1
	if offer := view.sandboxOffer; offer != nil {
		lines = append(lines, blank, chat.StyledLine(chat.ThemeGlyphs().Warning+" "+offer.warning, warning))
		if offer.alternative != nil {
			sandboxRow = len(lines)
			lines = append(lines, blank)
		} else {
			lines = append(lines, chat.StyledLine("No compatible sandbox is available. Check Runtimes in Settings.", muted))
		}
	}
	lines = append(lines, blank)
	if canAttach {
		lines = append(lines, chat.LineFrom(
			chat.StyledSpan("Attached directories: ", muted),
			chat.StyledSpan(strconv.Itoa(len(mounts.mounts)), chat.Title(false)),
		), blank)
	}

	subagents := view.subagents
	subagentRow, oracleRow, orchestratorsRow := -1, -1, -1
	if subagents != nil {
		lines = append(lines, blank)
		subagentRow = len(lines)
		lines = append(lines, blank)
		if reason, ok := subagents.unavailableReason(); ok {
			lines = append(lines, chat.RawLine(reason))
		}
		if subagents.oracleAvailable() {
			oracleRow = len(lines)
			lines = append(lines, blank)
		}
		if subagents.orchestratorsAvailable() {
			orchestratorsRow = len(lines)
			lines = append(lines, blank)
		}
	}
	lastControlRow := 0
	for _, row := range []int{worktreeRow, sandboxRow, subagentRow, oracleRow, orchestratorsRow} {
		if row >= 0 {
			lastControlRow = max(lastControlRow, row+1)
		}
	}
	if len(mounts.mounts) == 0 || !canAttach {
		for len(lines) > lastControlRow && lines[len(lines)-1].Width() == 0 {
			lines = lines[:len(lines)-1]
		}
	}
	popup := wizardModal(frame, surfaces, area)
	width := max(popup.Dx()-4, 0)
	var wrapped []chat.Line
	offsets := make([]int, 0, len(lines))
	for _, line := range lines {
		offsets = append(offsets, len(wrapped))
		wrapped = append(wrapped, chat.WrapLines([]chat.Line{line}, width)...)
	}
	mapRow := func(row int) int {
		if row < 0 {
			return -1
		}
		return offsets[row]
	}
	worktreeRow = mapRow(worktreeRow)
	sandboxRow = mapRow(sandboxRow)
	subagentRow = mapRow(subagentRow)
	oracleRow = mapRow(oracleRow)
	orchestratorsRow = mapRow(orchestratorsRow)
	summaryHeight := min(len(wrapped), dialogsMaxU16)
	listHeight := 0
	if canAttach {
		listHeight = len(mounts.mounts)
	}
	queueHeight := 0
	if view.queue != nil {
		queueHeight = 1 + len(view.queuedEntries) + len(view.preparedEntries)
	}
	totalHeight := summaryHeight + listHeight + queueHeight
	inner := chat.DialogShellPaddedInner(popup)
	titleLine := dismissibleModalTitle(&form.Form, popup, strings.TrimSpace(view.title), chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	body := chat.DialogShellLayout(inner, 1).Body
	focusedRow := -1
	if focused, ok := form.Focused(); ok {
		switch focused {
		case WizardControlCreateManagedWorktree:
			focusedRow = worktreeRow
		case WizardControlUseSandboxedTarget:
			focusedRow = sandboxRow
		case WizardControlSubagents:
			focusedRow = subagentRow
		case WizardControlOracle:
			focusedRow = oracleRow
		case WizardControlOrchestrators:
			focusedRow = orchestratorsRow
		case WizardControlReviewAttachments:
			focusedRow = summaryHeight + min(mounts.selectedMount, max(len(mounts.mounts)-1, 0))
		case WizardControlDiscardQueue:
			focusedRow = summaryHeight + listHeight
		}
	}
	var focusedBlock *chat.RowRange
	if focusedRow >= 0 {
		focusedBlock = &chat.RowRange{Start: focusedRow, End: focusedRow + 1}
	}
	viewport := form.Viewport(body, totalHeight, focusedBlock)
	for index, line := range wrapped {
		frame.RenderWidget(dialogsParagraph([]chat.Line{line}), viewport.Row(min(index, dialogsMaxU16), 1))
	}
	if worktree := view.worktree; worktree != nil && worktreeRow >= 0 {
		chat.CheckboxRender(
			frame,
			viewport.Row(worktreeRow, 1),
			"Create isolated checkout",
			worktree.create && worktree.available,
			worktree.available,
			&form.Form,
			WizardControlCreateManagedWorktree,
		)
	}
	if offer := view.sandboxOffer; sandboxRow >= 0 && offer != nil && offer.alternative != nil {
		id := nthKey(config.Targets, *offer.alternative)
		label := fmt.Sprintf("Use %s instead (sandboxed)", id)
		row := viewport.Row(sandboxRow, 1)
		buttonWidth := min(utf8.RuneCountInString(label)+4, dialogsMaxU16)
		form.RenderActions(
			frame,
			uv.Rect(row.Min.X, row.Min.Y, min(buttonWidth, row.Dx()), row.Dy()),
			[]chat.ButtonSpec[WizardControl]{{ID: WizardControlUseSandboxedTarget, Label: label, Enabled: true}},
		)
	}
	if subagents != nil && orchestratorsRow >= 0 {
		chat.CheckboxRender(
			frame,
			viewport.Row(orchestratorsRow, 1),
			"Orchestrators",
			subagents.policy.OrchestratorsEnabled(),
			true,
			&form.Form,
			WizardControlOrchestrators,
		)
	}
	var expandedSubagentCombo *wizardsReviewCombo
	if subagents != nil && subagentRow >= 0 {
		type selector struct {
			id        WizardControl
			label     string
			row       int
			values    []string
			committed int
			enabled   bool
		}
		selectors := []selector{{
			id:        WizardControlSubagents,
			label:     "Sub-agents",
			row:       subagentRow,
			values:    subagents.policies(),
			committed: subagents.policyIndex(),
			enabled:   true,
		}}
		if oracleRow >= 0 {
			committed := 0
			if !subagents.oracle {
				committed = 1
			}
			selectors = append(selectors, selector{
				id:        WizardControlOracle,
				label:     "Oracle",
				row:       oracleRow,
				values:    []string{"enabled", "disabled"},
				committed: committed,
				enabled:   true,
			})
		}
		_, unavailable := subagents.unavailableReason()
		for _, selector := range selectors {
			row := viewport.Row(selector.row, 1)
			labelWidth := min(11, row.Dx())
			frame.RenderWidget(chat.RawLine(selector.label), uv.Rect(row.Min.X, row.Min.Y, labelWidth, row.Dy()))
			field := uv.Rect(row.Min.X+labelWidth, row.Min.Y, row.Dx()-labelWidth, row.Dy())
			selected := subagents.combo.Selection(selector.id, selector.committed)
			value := ""
			if selected >= 0 && selected < len(selector.values) {
				value = selector.values[selected]
			}
			options := make([]chat.Line, 0, len(selector.values))
			for index, option := range selector.values {
				if subagents.isMjolnirIndex(index) && unavailable {
					options = append(options, chat.StyledLine(option, chat.Muted()))
				} else {
					options = append(options, chat.RawLine(option))
				}
			}
			chat.ComboBoxRender(
				frame,
				inner,
				field,
				value,
				options,
				selected,
				false,
				selector.enabled,
				" Values ",
				chat.PopupSideBelow,
				chat.ComboBoxDefaultMaxVisibleRows,
				&form.Form,
				selector.id,
			)
			form.SetRowEnabled(selector.id, subagents.enabledPolicies())
			if subagents.combo.IsOpen(selector.id) {
				expandedSubagentCombo = &wizardsReviewCombo{
					id:       selector.id,
					field:    field,
					value:    value,
					options:  options,
					selected: selected,
					enabled:  selector.enabled,
				}
			}
		}
	}
	if canAttach && len(mounts.mounts) > 0 {
		listArea := viewport.Row(summaryHeight, listHeight)
		rows := make([]chat.Line, 0, len(mounts.mounts))
		for _, mount := range mounts.mounts {
			rows = append(rows, chat.RawLine(fmt.Sprintf("%s → %s%s", mount.Source, mount.Destination, accessMarker(mount.Access))))
		}
		chat.ChoiceListRender(frame, listArea, rows, mounts.selectedMount, &form.Form, WizardControlReviewAttachments)
	}
	if queue := view.queue; queue != nil {
		queueArea := viewport.Row(summaryHeight+listHeight, 1)
		verb, when := "Start", "on resume"
		if queue.discard {
			verb = "Discard"
		}
		if moving {
			when = "after move"
		}
		chat.CheckboxRender(
			frame,
			queueArea,
			fmt.Sprintf("%s %s %s", verb, counted(queue.count, "queued command", "queued commands"), when),
			queue.discard,
			true,
			&form.Form,
			WizardControlDiscardQueue,
		)
		entryWidth := max(inner.Dx()-4, 0)
		for index, entry := range view.queuedEntries {
			text := "[empty command]"
			if strings.TrimSpace(entry.Text) != "" {
				text = strings.ReplaceAll(entry.Text, "\n", " ")
			}
			attachmentNote := ""
			switch count := len(entry.Attachments); count {
			case 0:
			case 1:
				attachmentNote = " · 1 attachment"
			default:
				attachmentNote = fmt.Sprintf(" · %d attachments", count)
			}
			text = truncateToCells(text, entryWidth, TruncateSummary)
			frame.RenderWidget(
				dialogsParagraph([]chat.Line{chat.StyledLine(
					fmt.Sprintf("  %d. %s%s", index+1, text, attachmentNote),
					mutedForeground,
				)}),
				viewport.Row(summaryHeight+listHeight+1+index, 1),
			)
		}
		for index, entry := range view.preparedEntries {
			kind, text := "prompt", ""
			switch command := entry.Kind.(type) {
			case core.QueuedCommandKindSetConfig:
				kind, text = "config", core.ConfigCommandText(command.Key, command.Value)
			default:
				text = core.MaterializedContentText(wizardsContentRaw(entry.Content))
			}
			if strings.TrimSpace(text) == "" {
				text = "[" + kind + "]"
			} else {
				text = kind + ": " + strings.ReplaceAll(text, "\n", " ")
			}
			text = truncateToCells(text, entryWidth, TruncateSummary)
			row := len(view.queuedEntries) + index
			frame.RenderWidget(
				dialogsParagraph([]chat.Line{chat.StyledLine(fmt.Sprintf("  %d. %s", row+1, text), mutedForeground)}),
				viewport.Row(summaryHeight+listHeight+1+row, 1),
			)
		}
	}
	_, ec2 := target.(core.TargetTemplateAwsEc2)
	buttons := []chat.ButtonSpec[WizardControl]{
		{ID: WizardControlCancel, Label: "Cancel", Enabled: true},
		{ID: WizardControlBack, Label: "Back", Enabled: true},
	}
	if canAttach {
		buttons = append(buttons, chat.ButtonSpec[WizardControl]{ID: WizardControlAdd, Label: "Add directory…", Enabled: true})
	}
	buttons = append(buttons, chat.ButtonSpec[WizardControl]{
		ID:    WizardControlSubmit,
		Label: view.submitLabel,
		Enabled: view.submitEnabled &&
			!view.remotePreflightInFlight &&
			(view.remoteRepositories != nil || !view.localChangesExcluded || view.remotePreflightError != nil) &&
			(view.allocation != nil || !ec2),
	})
	form.RenderActions(frame, chat.DialogShellLayout(inner, 1).Actions, buttons)
	// Paint the active popup last so it overlays the remaining review fields.
	if combo := expandedSubagentCombo; combo != nil {
		chat.ComboBoxRender(
			frame,
			inner,
			combo.field,
			combo.value,
			combo.options,
			combo.selected,
			true,
			combo.enabled,
			" Values ",
			chat.PopupSideBelow,
			chat.ComboBoxDefaultMaxVisibleRows,
			&form.Form,
			combo.id,
		)
		_, unavailable := subagents.unavailableReason()
		form.SetRowEnabled(combo.id, []bool{true, !unavailable})
	}
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
	config := dashboard.config.Get()
	targetID := nthKey(config.Targets, targetIndex)
	target, ok := config.Targets[targetID]
	if !ok {
		panic("selected target index is present in config")
	}
	var protection string
	switch target.(type) {
	case core.TargetTemplateAppleContainer, core.TargetTemplateSshAppleContainer:
		protection = "Apple Container has no :O overlay mode; each extra bind is read-only."
	case core.TargetTemplateLocalPodman, core.TargetTemplateSshPodman:
		protection = "Podman uses :O copy-on-write overlays; read-only skips the overlay."
	case core.TargetTemplateLocalDocker, core.TargetTemplateSshDocker:
		protection = "Docker uses session-owned OverlayFS volumes on the Docker host; read-only skips the overlay."
	case core.TargetTemplateAwsEc2:
		protection = "EC2 directories stream as tar.gz through one SSH connection into the destination."
	case core.TargetTemplateSshNitro:
		// Reachable when a wizard that already holds attachments moves to a
		// sandbox: the launch drops them and says so.
		protection = "Nitro sandboxes cannot attach host directories; these are dropped when the session starts."
	default:
		panic("bare targets do not attach resources")
	}
	lines := []chat.Line{
		chat.RawLine(fmt.Sprintf("Target: %s (%s)", targetID, targetLabel(target))),
		chat.RawLine(""),
		chat.StyledLine(protection, chat.Style{}.Foreground(chat.ThemePalette().Warning)),
	}
	if len(mounts.mounts) > 0 {
		lines = append(lines, chat.RawLine(""), chat.RawLine("Already attached:"))
		for _, mount := range mounts.mounts {
			lines = append(lines, chat.RawLine(fmt.Sprintf(
				"  %s → %s%s",
				mount.Source,
				mount.Destination,
				accessMarker(mount.Access),
			)))
		}
	}
	if mounts.error != nil {
		lines = append(lines, chat.RawLine(""), chat.StyledLine(*mounts.error, chat.Style{}.Foreground(chat.ThemePalette().Error)))
	}

	lines = append(lines, chat.RawLine(""))
	popup := wizardModal(frame, surfaces, area)
	lines = chat.WrapLines(lines, max(popup.Dx()-4, 0))
	infoHeight := min(len(lines), dialogsMaxU16)
	history := mounts.availableHistory()
	historyHeight := 0
	if len(history) > 0 {
		historyHeight = 2 + min(len(history), 5)
	}
	totalHeight := infoHeight + 5 + historyHeight
	inner := chat.DialogShellPaddedInner(popup)
	titleLine := dismissibleModalTitle(&form.Form, popup, strings.TrimSpace(title), chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	body := chat.DialogShellLayout(inner, 1).Body
	var focusedRow *chat.RowRange
	if focused, ok := form.Focused(); ok {
		row := -1
		switch focused {
		case WizardControlMountSource:
			row = infoHeight
		case WizardControlMountDestination:
			row = infoHeight + 2
		case WizardControlMountAccess:
			row = infoHeight + 4
		case WizardControlMountHistory:
			row = infoHeight + 7 + min(mounts.historySelection, 4)
		}
		if row >= 0 {
			focusedRow = &chat.RowRange{Start: row, End: row + 1}
		}
	}
	viewport := form.Viewport(body, totalHeight, focusedRow)
	for index, line := range lines {
		frame.RenderWidget(dialogsParagraph([]chat.Line{line}), viewport.Row(min(index, dialogsMaxU16), 1))
	}
	// A completion popup may cover the rows below its field, but not the
	// button row and not the dialog's frame.
	popupBounds := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), body.Dy())
	fieldWidth := max(inner.Dx()-12, 0)
	sourceRow := viewport.Row(infoHeight, 1)
	frame.RenderWidget(dialogsTextParagraph("Source:"), uv.Rect(sourceRow.Min.X, sourceRow.Min.Y, min(10, sourceRow.Dx()), sourceRow.Dy()))
	sourceField := uv.Rect(sourceRow.Min.X+10, sourceRow.Min.Y, fieldWidth, sourceRow.Dy())
	chat.PathFieldRenderWithin(frame, popupBounds, sourceField, &mounts.source, &form.Form, WizardControlMountSource)
	destinationRow := viewport.Row(infoHeight+2, 1)
	frame.RenderWidget(
		dialogsTextParagraph("Destination:"),
		uv.Rect(destinationRow.Min.X, destinationRow.Min.Y, min(10, destinationRow.Dx()), destinationRow.Dy()),
	)
	chat.PathFieldRenderWithin(
		frame,
		popupBounds,
		uv.Rect(destinationRow.Min.X+10, destinationRow.Min.Y, fieldWidth, destinationRow.Dy()),
		&mounts.destination,
		&form.Form,
		WizardControlMountDestination,
	)
	accessRow := viewport.Row(infoHeight+4, 1)
	frame.RenderWidget(dialogsTextParagraph("Access:"), uv.Rect(accessRow.Min.X, accessRow.Min.Y, min(10, accessRow.Dx()), accessRow.Dy()))
	accessField := uv.Rect(accessRow.Min.X+10, accessRow.Min.Y, fieldWidth, accessRow.Dy())
	choices := mounts.accessChoices()
	renderAccessCombo(frame, inner, accessField, mounts.access, choices, &mounts.accessCombo, false, &form.Form, WizardControlMountAccess)
	if len(history) > 0 {
		label := viewport.Row(infoHeight+6, 1)
		frame.RenderWidget(dialogsParagraph([]chat.Line{chat.StyledLine("Recent on this host:", chat.Muted())}), label)
		rows := make([]chat.Line, 0, len(history))
		for _, path := range history {
			rows = append(rows, chat.RawLine(path))
		}
		list := viewport.Row(infoHeight+7, min(len(rows), 5))
		chat.ChoiceListRender(frame, list, rows, min(mounts.historySelection, len(rows)-1), &form.Form, WizardControlMountHistory)
	}
	form.RenderActions(frame, chat.DialogShellLayout(inner, 1).Actions, []chat.ButtonSpec[WizardControl]{
		{ID: WizardControlCancel, Label: "Cancel", Enabled: true},
		{ID: WizardControlBack, Label: "Back", Enabled: true},
		{ID: WizardControlAdd, Label: "Add directory", Enabled: true},
	})
	// The completion popup hangs over the rows below the source, so it is
	// drawn again once those rows are on the screen.
	if focused, ok := form.Focused(); ok && focused == WizardControlMountSource {
		chat.PathFieldRenderWithin(frame, popupBounds, sourceField, &mounts.source, &form.Form, WizardControlMountSource)
	}
	if mounts.accessCombo.IsOpen(WizardControlMountAccess) {
		renderAccessCombo(frame, inner, accessField, mounts.access, choices, &mounts.accessCombo, true, &form.Form, WizardControlMountAccess)
	}
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
	form := &wizard.form
	initial := stepInitial(wizard.step)
	beginFormFrame(form, initial)
	if wizard.step == WizardStepLaunching {
		title := "Opening session"
		if wizard.moving {
			title = "Moving session"
		}
		renderLaunching(frame, area, form, surfaces, title, wizardsLaunchError(wizard.LaunchError()))
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepReview {
		renderResumeWizardReview(frame, area, dashboard, wizard, surfaces)
		form.EndFrame(initial)
		return
	}
	if wizard.step == WizardStepMounts {
		renderMountWizard(frame, area, dashboard, wizard.target, &wizard.mounts, form, " Add attached directory ", surfaces)
		form.EndFrame(initial)
		return
	}
	targetHidden := targetStepHidden(dashboard, wizard)
	var title string
	var choices []PickerChoice
	var selected int
	var help []chat.Line
	var control WizardControl
	switch wizard.step {
	case WizardStepProfile:
		profiles := dashboard.resumeWizardProfiles(wizard)
		var sessionHarness *core.HarnessKind
		if session := dashboard.SessionRecord(wizard.sessionID); session != nil {
			harness := session.HarnessKind
			sessionHarness = &harness
		}
		rows := make([]PickerChoice, 0, len(profiles))
		for _, profile := range profiles {
			choice := dashboard.profileChoice(profile.ID, profile.Kind)
			if sessionHarness != nil && *sessionHarness != profile.Kind {
				choice = choice.withNote("(lossy: text-only transcript)")
			}
			rows = append(rows, choice)
		}
		// Only a change of harness hands over a text transcript. The
		// footnote describes the selected profile, so a same-harness resume
		// is not warned about a loss it does not have.
		selectedIsLossy := wizard.profile >= 0 && wizard.profile < len(profiles) &&
			sessionHarness != nil && *sessionHarness != profiles[wizard.profile].Kind
		if selectedIsLossy {
			help = append(help, pickerHelp("Lossy: text only; tool calls + reasoning dropped."))
		} else if sessionHarness != nil {
			help = append(help, pickerHelp("Same agent: the conversation continues natively."))
		}
		help = append(help, missingProfileHelp(dashboard)...)
		step := fmt.Sprintf("%s profile (cross-harness supported)", stepCounter(1, 3, targetHidden, wizard))
		title = resumeWizardTitle(wizard, step, step)
		choices = profileTable(rows)
		selected = wizard.profile
		control = WizardControlProfileList
	case WizardStepTarget:
		choices, help, selected = targetStepChoices(dashboard, wizard, wizard.sizingError)
		step := fmt.Sprintf("%s new target", stepCounter(2, 3, targetHidden, wizard))
		title = resumeWizardTitle(wizard, step, step)
		control = WizardControlTargetList
	default:
		panic("resume picker step has a list control")
	}
	if wizard.source == ResumeSourceArchive {
		// An archived session has no record to read a name from, so the
		// title the index kept is shown beside the choices.
		help = append([]chat.Line{pickerHelp("Restoring: " + wizard.title)}, help...)
	}
	renderPicker(
		frame,
		area,
		title,
		choices,
		help,
		PickerNavigation{
			resources:   targetResources(dashboard, wizard),
			hasBack:     wizard.HasBack(),
			selected:    selected,
			control:     control,
			nextEnabled: wizard.step != WizardStepTarget || targetAdvanceEnabled(dashboard, wizard),
		},
		form,
		surfaces,
	)
	form.EndFrame(control)
}

// renderResumeWizardReview draws the resume wizard's review step (the Review
// arm of Rust's render_resume_wizard).
func renderResumeWizardReview(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, wizard *ResumeWizard, surfaces *chat.FrameSurfaces) {
	config := dashboard.config.Get()
	profiles := dashboard.resumeWizardProfiles(wizard)
	profileID := "unknown"
	if wizard.profile >= 0 && wizard.profile < len(profiles) {
		profileID = profiles[wizard.profile].ID
	}
	session := dashboard.SessionRecord(wizard.sessionID)
	bundleID := "unknown"
	if session != nil {
		bundleID = session.BundleID
	}
	targetID := nthKey(config.Targets, wizard.target)
	var reusedProjectDirectory *string
	if session != nil {
		checkout, err := dashboard.state.Get().Checkout(session.ID)
		if err != nil {
			checkout = session.RecordCheckout()
		}
		if plan, err := client.ResumeCompatibilityWithCheckout(session, checkout, config, targetID); err == nil && plan == client.ResumePlanInPlace {
			if directory, ok := checkout.ProjectDirectory(); ok {
				reusedProjectDirectory = &directory
			}
		}
	}
	// An archived session has no record here, so its own title is what
	// identifies it; a plain resume names the project it reopens.
	projectLabel, project, projectNote := "Project", bundleID, ""
	switch {
	case wizard.source == ResumeSourceArchive:
		projectLabel, project = "Archived session", wizard.title
	case reusedProjectDirectory != nil:
		projectLabel, project, projectNote = "Project directory", *reusedProjectDirectory, " (reused)"
	}
	counter := stepCounter(3, 3, targetStepHidden(dashboard, wizard), wizard)
	reviewTitle := resumeWizardTitle(wizard, counter+" review", counter+" confirm")
	submitLabel := "Resume"
	switch {
	case wizard.moving && wizard.preparationError != nil:
		submitLabel = "Retry"
	case wizard.moving:
		submitLabel = "Move"
	case wizard.source == ResumeSourceArchive:
		submitLabel = "Restore"
	}
	_, subagentError := wizard.subagents.error()
	preparation := wizard.preparation
	var subagents *SubagentDraft
	if wizard.subagentChoiceApplies(dashboard) {
		subagents = wizard.subagents
	}
	stoppedSubagents := 0
	if wizard.moving && preparation != nil && preparation.StopsSubagents {
		stoppedSubagents = dashboard.verdict(wizard.sessionID).UnfinishedChildren
	}
	activeInterruption := wizard.moving
	if preparation != nil {
		activeInterruption = preparation.Active
	}
	var clearResourceAllocation bool
	if preparation != nil {
		clearResourceAllocation = preparation.Selection.ClearResourceAllocation
	} else if wizard.moving {
		record := dashboard.SessionRecord(wizard.sessionID)
		if record != nil && record.ResourceAllocation != nil {
			switch config.Targets[targetID].(type) {
			case core.TargetTemplateLocalBare, core.TargetTemplateSshBare:
				clearResourceAllocation = true
			}
		}
	}
	var queue *reviewWizardQueue
	var queuedEntries []core.QueuedPrompt
	var preparedEntries []core.MaterializedQueuedPrompt
	var conversion *core.RawConversionPreview
	if preparation != nil {
		if len(preparation.QueuedCommands) > 0 {
			queue = &reviewWizardQueue{count: len(preparation.QueuedCommands), discard: wizard.discardQueue}
		}
		preparedEntries = preparation.QueuedCommands
		conversion = preparation.Conversion
	} else if detail := (*dashboard.sessionDetails.Get())[wizard.sessionID]; detail != nil {
		if count := len(detail.queuedPrompts); count > 0 {
			queue = &reviewWizardQueue{count: count, discard: wizard.discardQueue}
		}
		queuedEntries = detail.queuedPrompts
	}
	renderReviewWizard(
		frame,
		area,
		dashboard,
		ReviewWizardView{
			profileID:        profileID,
			projectLabel:     projectLabel,
			project:          project,
			projectNote:      projectNote,
			targetID:         targetID,
			allocation:       wizard.resourceAllocation,
			mounts:           &wizard.mounts,
			title:            reviewTitle,
			submitLabel:      submitLabel,
			moving:           wizard.moving,
			preparing:        wizard.preparing,
			preparationError: wizard.preparationError,
			submitEnabled: (wizard.subagentChange(dashboard) == nil || !subagentError) &&
				(!wizard.moving || preparation != nil || wizard.preparationError != nil),
			sourceUnavailable:  preparation != nil && preparation.SourceUnavailable,
			subagents:          subagents,
			stoppedSubagents:   stoppedSubagents,
			activeInterruption: activeInterruption,
			destinationChecksPending: preparation != nil &&
				preparation.DestinationChecks == core.DestinationChecksAfterProvisioning,
			inPlaceMove:             preparation != nil && preparation.InPlace,
			clearResourceAllocation: clearResourceAllocation,
			queue:                   queue,
			queuedEntries:           queuedEntries,
			preparedEntries:         preparedEntries,
			conversion:              conversion,
			sandboxOffer:            dashboard.sandboxOffer(wizard),
		},
		&wizard.form,
		surfaces,
	)
}
