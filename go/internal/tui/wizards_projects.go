// Port of mj-tui/src/wizards/projects.rs.

// Project discovery state and controls. All filesystem and network work is
// requested as dashboard actions; this module only edits the draft.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"sync/atomic"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProjectTab is the project picker's source: recent projects, GitHub, or a
// URL or path. The zero value is Recent.
type ProjectTab int

const (
	ProjectTabRecent ProjectTab = iota
	ProjectTabGithub
	ProjectTabUrl
)

// ProjectPicker is the new-session wizard's project discovery state.
type ProjectPicker struct {
	tab           ProjectTab
	query         chat.PathInput
	entries       []core.ProjectEntry
	selected      int
	truncated     bool
	error         *string
	creationError *string
	multiple      bool
	loading       bool
	request       core.ProjectDiscoveryRequest
	pending       bool
	focusResults  bool
	retryCatalog  bool
	id            uint64
	generation    uint64
}

// nextProjectPickerID hands every picker its own id; the first is 1.
var nextProjectPickerID atomic.Uint64

// defaultProjectPicker is ProjectPicker's Default: an empty Recent picker
// with a fresh id.
func defaultProjectPicker() ProjectPicker {
	return ProjectPicker{
		tab:   ProjectTabRecent,
		query: chat.NewPathInput(),
		id:    nextProjectPickerID.Add(1),
	}
}

// equal is Rust's derived PartialEq for ProjectPicker.
func (p *ProjectPicker) equal(other *ProjectPicker) bool {
	return p.tab == other.tab &&
		p.query.Equal(&other.query) &&
		slices.Equal(p.entries, other.entries) &&
		p.selected == other.selected &&
		p.truncated == other.truncated &&
		wizardOptionalStringsEqual(p.error, other.error) &&
		wizardOptionalStringsEqual(p.creationError, other.creationError) &&
		p.multiple == other.multiple &&
		p.loading == other.loading &&
		p.request == other.request &&
		p.pending == other.pending &&
		p.focusResults == other.focusResults &&
		p.retryCatalog == other.retryCatalog &&
		p.id == other.id &&
		p.generation == other.generation
}

// wizardOptionalStringsEqual compares two Rust `Option<String>` values.
func wizardOptionalStringsEqual(left, right *string) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func (p *ProjectPicker) githubQueryNeedsSearch() bool {
	if p.tab != ProjectTabGithub {
		return false
	}
	request, ok := p.request.(core.ProjectDiscoveryRequestGithub)
	return !ok || request.Query != strings.TrimSpace(p.query.Value())
}

func (p *ProjectPicker) githubQueryChanged() {
	if p.tab == ProjectTabGithub {
		p.generation++
		p.entries = nil
		p.selected = 0
		p.loading = false
		p.pending = false
		p.error = nil
		p.creationError = nil
		p.truncated = false
		p.focusResults = false
		p.request = nil
	}
}

func (p *ProjectPicker) context() string {
	return fmt.Sprintf("%d:%d", p.id, p.generation)
}

func (p *ProjectPicker) load(request core.ProjectDiscoveryRequest) {
	p.generation++
	p.request = request
	p.pending = true
	p.focusResults = true
	p.loading = true
	p.error = nil
	p.entries = nil
	p.selected = 0
	p.truncated = false
}

func (w *NewWizard) projectCanUse() bool {
	picker := w.projectPicker
	return picker.githubQueryNeedsSearch() ||
		len(w.newBundleRepositories) > 0 ||
		(picker.tab == ProjectTabUrl && strings.TrimSpace(w.newBundleSource.Value()) != "") ||
		(!picker.multiple && len(picker.entries) > 0)
}

func (w *NewWizard) openProjects(dashboard *DashboardState) {
	w.step = WizardStepNewBundle
	w.chooseProjectTab(dashboard, ProjectTabRecent)
}

func (w *NewWizard) chooseProjectTab(dashboard *DashboardState, tab ProjectTab) {
	picker := w.projectPicker
	picker.tab = tab
	picker.generation++
	picker.loading = false
	picker.pending = false
	picker.error = nil
	picker.creationError = nil
	picker.entries = nil
	picker.selected = 0
	picker.truncated = false
	switch tab {
	case ProjectTabRecent:
		picker.entries = dashboard.recentProjectEntries()
		picker.retryCatalog = false
		picker.pending = true
		picker.request = nil
	case ProjectTabGithub:
		picker.load(core.ProjectDiscoveryRequestGithub{Query: strings.TrimSpace(picker.query.Value())})
	}
	if tab == ProjectTabGithub {
		picker.focusResults = false
	}
	var initial WizardControl
	switch {
	case tab == ProjectTabUrl:
		initial = WizardControlNewBundleSource
	case tab == ProjectTabGithub:
		initial = WizardControlProjectQuery
	case len(picker.entries) > 0:
		initial = WizardControlProjectResults
	default:
		initial = WizardControlProjectGithub
	}
	form := &w.form
	form.BeginUpdate()
	w.declareProjectControls(form)
	form.EndFrame(initial)
	form.Focus(initial)
}

func (w *NewWizard) declareProjectControls(form *chat.Dialog[WizardControl]) {
	picker := w.projectPicker
	ready := !w.bundleCreationInFlight
	for _, id := range []WizardControl{WizardControlProjectRecent, WizardControlProjectGithub, WizardControlProjectUrl} {
		form.DeclareWithEnabled(id, chat.ControlKindButton{}, ready)
	}
	if picker.tab == ProjectTabGithub {
		form.DeclareWithEnabled(WizardControlProjectQuery, chat.ControlKindTextField{}, ready)
		form.DeclareWithEnabled(WizardControlProjectSearch, chat.ControlKindButton{}, ready)
	}
	if picker.error != nil || picker.creationError != nil {
		form.DeclareWithEnabled(WizardControlProjectRetry, chat.ControlKindButton{}, ready)
	}
	if picker.tab == ProjectTabUrl {
		form.DeclareWithEnabled(WizardControlNewBundleSource, w.newBundleSource.ControlKind(), ready)
	} else {
		form.DeclareWithEnabled(
			WizardControlProjectResults,
			chat.ControlKindChoiceList{Len: len(picker.entries), Selected: picker.selected},
			ready && !picker.loading && len(picker.entries) > 0,
		)
	}
	form.DeclareWithEnabled(WizardControlProjectMultiple, chat.ControlKindCheckbox{}, ready)
	if picker.multiple {
		form.DeclareWithEnabled(
			WizardControlNewBundleRepositories,
			chat.ControlKindChoiceList{Len: len(w.newBundleRepositories), Selected: w.newBundleSelected},
			ready && len(w.newBundleRepositories) > 0,
		)
		form.DeclareWithEnabled(WizardControlProjectMakePrimary, chat.ControlKindButton{}, ready && len(w.newBundleRepositories) > 1)
		form.DeclareWithEnabled(WizardControlNewBundleRemove, chat.ControlKindButton{}, ready && len(w.newBundleRepositories) > 0)
		if picker.tab == ProjectTabUrl {
			form.DeclareWithEnabled(WizardControlAdd, chat.ControlKindButton{}, ready && strings.TrimSpace(w.newBundleSource.Value()) != "")
		}
	}
	form.DeclareWithEnabled(WizardControlBack, chat.ControlKindButton{}, ready)
	form.DeclareWithEnabled(WizardControlCancel, chat.ControlKindButton{}, ready)
	form.DeclareWithEnabled(WizardControlNext, chat.ControlKindButton{}, ready && w.projectCanUse())
	form.SetDefaultAction(WizardControlNext)
}

// ProjectCatalogContext identifies the open project picker's catalog
// request, when the new-session wizard is open.
func (d *DashboardState) ProjectCatalogContext() (string, bool) {
	if wizard, ok := d.mode.(ModeNew); ok {
		return wizard.projectPicker.context(), true
	}
	return "", false
}

func (d *DashboardState) recentProjectEntries() []core.ProjectEntry {
	var paths []string
	if d.launchProjectDirectory != nil {
		paths = append(paths, *d.launchProjectDirectory)
	}
	paths = append(paths, d.state.Get().ProjectDirectories("local")...)
	var entries []core.ProjectEntry
	for _, path := range paths {
		source := path
		if slices.ContainsFunc(entries, func(entry core.ProjectEntry) bool { return entry.Source == source }) {
			continue
		}
		current := d.launchProjectDirectory != nil && *d.launchProjectDirectory == path
		name, ok := resumePathFileName(path)
		if !ok {
			name = path
		}
		description := source
		if current {
			description = "Current project · " + source
		}
		entries = append(entries, core.ProjectEntry{
			Name:        name,
			Source:      source,
			Description: description,
			Kind:        core.ProjectEntryKindRepository,
		})
	}
	return entries
}

func (d *DashboardState) ApplyProjectCatalogStatus(status core.ProjectCatalogStatus) {
	wizard, ok := d.mode.(ModeNew)
	if !ok {
		return
	}
	if wizard.step != WizardStepNewBundle || wizard.projectPicker.tab != ProjectTabRecent {
		return
	}
	wizard.projectPicker.loading = false
	wizard.projectPicker.error = status.FailureSummary()
}

// TakeProjectDiscovery is the discovery the open project picker is waiting
// to have sent, if any.
func (d *DashboardState) TakeProjectDiscovery() DashboardAction {
	wizard, ok := d.mode.(ModeNew)
	if !ok {
		return nil
	}
	picker := wizard.projectPicker
	if wizard.step != WizardStepNewBundle || !picker.pending {
		return nil
	}
	picker.pending = false
	switch {
	case picker.request != nil:
		return DashboardActionDiscoverProjects{Context: picker.context(), Request: picker.request}
	case picker.retryCatalog:
		picker.retryCatalog = false
		return DashboardActionRefreshProjects{Retry: true}
	}
	return DashboardActionLoadMountHistory{}
}

// ApplyProjectDiscovery applies a discovery answer (Rust's
// `Result<ProjectDiscovery, String>`: err set for Err) to the picker that
// asked for it.
func (d *DashboardState) ApplyProjectDiscovery(context string, result core.ProjectDiscovery, err error) {
	wizard, ok := d.mode.(ModeNew)
	if !ok {
		return
	}
	if wizard.step != WizardStepNewBundle || wizard.projectPicker.context() != context {
		return
	}
	picker := wizard.projectPicker
	picker.loading = false
	if err != nil {
		text := err.Error()
		picker.error = &text
		return
	}
	picker.entries = result.Entries
	picker.truncated = result.Truncated
	picker.selected = 0
	picker.error = nil
	// A late result must not steal focus from an edited search.
	if picker.focusResults && len(picker.entries) > 0 {
		wizard.form.Focus(WizardControlProjectResults)
	}
	picker.focusResults = false
}

func (d *DashboardState) activateProjectControl(wizard *NewWizard, id WizardControl) DashboardAction {
	picker := wizard.projectPicker
	if id == WizardControlNext && picker.githubQueryNeedsSearch() {
		id = WizardControlProjectSearch
	}
	switch id {
	case WizardControlProjectRecent:
		wizard.chooseProjectTab(d, ProjectTabRecent)
	case WizardControlProjectGithub:
		wizard.chooseProjectTab(d, ProjectTabGithub)
	case WizardControlProjectUrl:
		wizard.chooseProjectTab(d, ProjectTabUrl)
	case WizardControlProjectQuery, WizardControlProjectSearch:
		picker.load(core.ProjectDiscoveryRequestGithub{Query: strings.TrimSpace(picker.query.Value())})
		wizard.form.Focus(WizardControlProjectResults)
	case WizardControlProjectMakePrimary:
		if wizard.newBundleSelected < len(wizard.newBundleRepositories) {
			source := wizard.newBundleRepositories[wizard.newBundleSelected]
			wizard.newBundleRepositories = slices.Delete(wizard.newBundleRepositories, wizard.newBundleSelected, wizard.newBundleSelected+1)
			wizard.newBundleRepositories = slices.Insert(wizard.newBundleRepositories, 0, source)
			wizard.newBundleSelected = 0
		}
	case WizardControlProjectRetry:
		if picker.creationError != nil {
			return d.submitNewBundle(wizard)
		}
		if picker.request != nil {
			picker.load(picker.request)
		} else if picker.tab == ProjectTabRecent {
			picker.generation++
			picker.retryCatalog = true
			picker.pending = true
			picker.loading = true
			picker.error = nil
		}
	case WizardControlProjectResults, WizardControlNext:
		if id == WizardControlNext && len(wizard.newBundleRepositories) > 0 {
			return d.submitNewBundle(wizard)
		}
		if picker.tab == ProjectTabUrl {
			return d.submitNewBundle(wizard)
		}
		if picker.selected >= 0 && picker.selected < len(picker.entries) {
			entry := picker.entries[picker.selected]
			if picker.multiple {
				if index := slices.Index(wizard.newBundleRepositories, entry.Source); index >= 0 {
					wizard.newBundleRepositories = slices.Delete(wizard.newBundleRepositories, index, index+1)
				} else {
					wizard.newBundleRepositories = append(wizard.newBundleRepositories, entry.Source)
				}
				wizard.newBundleSelected = max(len(wizard.newBundleRepositories)-1, 0)
			} else {
				wizard.newBundleSource.SetValue(entry.Source)
				return d.submitNewBundle(wizard)
			}
		}
	}
	d.mode = ModeNew{wizard}
	return DashboardActionNone{}
}

func renderProjectPicker(frame *chat.Frame, area uv.Rectangle, wizard *NewWizard, form *chat.Dialog[WizardControl], surfaces *chat.FrameSurfaces) {
	picker := wizard.projectPicker
	popup := wizardModal(frame, surfaces, area)
	inner := chat.DialogShellPaddedInner(popup)
	layout := chat.DialogShellLayout(inner, 1)
	ready := !wizard.bundleCreationInFlight
	title := dismissibleModalTitle(&form.Form, popup, "Choose a project", chat.Title(true), ready)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	var descriptionText string
	switch picker.tab {
	case ProjectTabRecent:
		descriptionText = "Pick up a recent project, or find another on GitHub or by URL or path."
	case ProjectTabGithub:
		descriptionText = "Your GitHub repositories. Search by name or owner/repository."
	default:
		descriptionText = "Git URL, GitHub owner/repo, or local path. HTTP credentials are removed."
	}
	description := chat.WrapLines([]chat.Line{chat.StyledLine(descriptionText, chat.Muted())}, inner.Dx())
	inputRow := len(description) + 3
	listRow := inputRow
	switch picker.tab {
	case ProjectTabGithub:
		listRow = inputRow + 4
	case ProjectTabUrl:
		listRow = inputRow + 2
	}
	errorText := picker.creationError
	if errorText == nil {
		errorText = picker.error
	}
	var messageLines []chat.Line
	switch {
	case picker.loading:
		messageLines = []chat.Line{chat.RawLine("Loading projects…")}
	case errorText != nil:
		messageLines = []chat.Line{chat.RawLine(*errorText)}
	case len(picker.entries) == 0 && picker.tab != ProjectTabUrl:
		empty := "No projects found."
		switch picker.tab {
		case ProjectTabRecent:
			empty = "No recent projects yet. Choose GitHub or URL or path above to get started."
		case ProjectTabGithub:
			empty = "No matching repositories. Try another search or paste a repository URL."
		}
		messageLines = []chat.Line{chat.RawLine(empty)}
	}
	message := chat.WrapLines(messageLines, inner.Dx())
	listHeight := 0
	switch {
	case len(message) > 0:
		listHeight = len(message)
	case picker.tab == ProjectTabUrl:
		listHeight = 0
	default:
		listHeight = min(max(len(picker.entries), 1), 5)
	}
	boolRows := func(value bool) int {
		if value {
			return 1
		}
		return 0
	}
	retryRow := listRow + listHeight + 1
	var statusLines []chat.Line
	switch {
	case wizard.bundleCreationInFlight:
		statusLines = []chat.Line{chat.StyledLine("Preparing project…", chat.Muted())}
	case picker.truncated:
		statusLines = []chat.Line{chat.StyledLine("Showing the first matches. Search or browse further to narrow the list.", chat.Muted())}
	}
	status := chat.WrapLines(statusLines, inner.Dx())
	statusRow := listRow + listHeight + boolRows(listHeight > 0)
	if errorText != nil {
		statusRow = retryRow + 2
	}
	multipleRow := statusRow
	if len(status) > 0 {
		multipleRow += len(status) + 1
	}
	basketRow := multipleRow + 2
	basketHeight := min(len(wizard.newBundleRepositories), 3)
	repoActionsRow := basketRow + basketHeight + boolRows(basketHeight > 0)
	height := multipleRow + 1
	if picker.multiple {
		height = repoActionsRow + 1
	}
	var focusedRow *int
	if focused, ok := form.Focused(); ok {
		row := -1
		switch focused {
		case WizardControlProjectRecent, WizardControlProjectGithub, WizardControlProjectUrl:
			row = 0
		case WizardControlProjectQuery, WizardControlNewBundleSource:
			row = inputRow
		case WizardControlProjectSearch:
			row = inputRow + 2
		case WizardControlProjectResults:
			row = listRow + max(listHeight-1, 0)
		case WizardControlProjectRetry:
			row = retryRow
		case WizardControlProjectMultiple:
			row = multipleRow
		case WizardControlNewBundleRepositories:
			row = basketRow + max(basketHeight-1, 0)
		case WizardControlAdd, WizardControlProjectMakePrimary, WizardControlNewBundleRemove:
			row = repoActionsRow
		}
		if row >= 0 {
			focusedRow = &row
		}
	}
	var focusedBlock *chat.RowRange
	if focusedRow != nil {
		focusedBlock = &chat.RowRange{Start: *focusedRow, End: *focusedRow + 1}
	}
	viewport := form.Viewport(layout.Body, height, focusedBlock)
	row := func(start int) uv.Rectangle { return viewport.Row(start, 1) }
	tabs := []struct {
		id    WizardControl
		label string
		tab   ProjectTab
	}{
		{WizardControlProjectRecent, "Recent", ProjectTabRecent},
		{WizardControlProjectGithub, "GitHub", ProjectTabGithub},
		{WizardControlProjectUrl, "URL or Path", ProjectTabUrl},
	}
	var actions []chat.ButtonSpec[WizardControl]
	for _, tab := range tabs {
		label := tab.label
		if tab.tab == picker.tab {
			marker := "●"
			if chat.ASCII() {
				marker = "*"
			}
			label = marker + label
		}
		actions = append(actions, chat.ButtonSpec[WizardControl]{ID: tab.id, Label: label, Enabled: ready})
	}
	form.RenderActions(frame, row(0), actions)
	// Render wrapped text one row at a time so partial viewport clipping
	// keeps the text and control geometry on the same rows.
	for index, line := range description {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), row(2+index))
	}
	switch picker.tab {
	case ProjectTabGithub:
		chat.PathFieldRenderWithin(frame, layout.Body, row(inputRow), &picker.query, &form.Form, WizardControlProjectQuery)
		form.RenderActions(frame, row(inputRow+2), []chat.ButtonSpec[WizardControl]{
			{ID: WizardControlProjectSearch, Label: "Search", Enabled: ready},
		})
	case ProjectTabUrl:
		chat.PathFieldRenderWithin(frame, layout.Body, row(inputRow), &wizard.newBundleSource, &form.Form, WizardControlNewBundleSource)
	}
	listArea := viewport.Row(listRow, listHeight)
	if len(message) > 0 {
		for index, line := range message {
			frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), row(listRow+index))
		}
	} else if picker.tab != ProjectTabUrl {
		rows := make([]chat.Line, 0, len(picker.entries))
		for _, entry := range picker.entries {
			marker := " "
			switch {
			case entry.Kind == core.ProjectEntryKindDirectory:
				marker = "▸"
				if chat.ASCII() {
					marker = ">"
				}
			case slices.Contains(wizard.newBundleRepositories, entry.Source):
				marker = "✓"
				if chat.ASCII() {
					marker = "+"
				}
			}
			rows = append(rows, chat.LineFrom(
				chat.RawSpan(fmt.Sprintf("%s %s  ", marker, entry.Name)),
				chat.StyledSpan(entry.Description, chat.Muted()),
			))
		}
		chat.ChoiceListRender(frame, listArea, rows, picker.selected, &form.Form, WizardControlProjectResults)
	}
	if errorText != nil {
		form.RenderActions(frame, row(retryRow), []chat.ButtonSpec[WizardControl]{
			{ID: WizardControlProjectRetry, Label: "Retry", Enabled: ready},
		})
	}
	for index, line := range status {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), row(statusRow+index))
	}
	chat.CheckboxRender(frame, row(multipleRow), "Use several repositories together", picker.multiple, ready, &form.Form, WizardControlProjectMultiple)
	if picker.multiple {
		rows := make([]chat.Line, 0, len(wizard.newBundleRepositories))
		for index, source := range wizard.newBundleRepositories {
			role := "Also"
			if index == 0 {
				role = "Main"
			}
			rows = append(rows, chat.RawLine(fmt.Sprintf("%s  %s", role, source)))
		}
		chat.ChoiceListRender(frame, viewport.Row(basketRow, basketHeight), rows, wizard.newBundleSelected, &form.Form, WizardControlNewBundleRepositories)
		form.RenderActions(frame, row(repoActionsRow), []chat.ButtonSpec[WizardControl]{
			{
				ID:      WizardControlAdd,
				Label:   "Add repository",
				Enabled: ready && picker.tab == ProjectTabUrl && strings.TrimSpace(wizard.newBundleSource.Value()) != "",
			},
			{ID: WizardControlProjectMakePrimary, Label: "Make main", Enabled: ready && len(wizard.newBundleRepositories) > 1},
			{ID: WizardControlNewBundleRemove, Label: "Remove", Enabled: ready && len(wizard.newBundleRepositories) > 0},
		})
	}
	useLabel := "Use project"
	switch {
	case wizard.bundleCreationInFlight:
		useLabel = "Preparing…"
	case picker.multiple && len(wizard.newBundleRepositories) > 0:
		useLabel = "Use " + counted(len(wizard.newBundleRepositories), "repository", "repositories")
	}
	form.RenderActions(frame, layout.Actions, []chat.ButtonSpec[WizardControl]{
		{ID: WizardControlBack, Label: "Back", Enabled: ready},
		{ID: WizardControlCancel, Label: "Cancel", Enabled: ready},
		{ID: WizardControlNext, Label: useLabel, Enabled: ready && wizard.projectCanUse()},
	})
	wizard.declareProjectControls(form)
}
