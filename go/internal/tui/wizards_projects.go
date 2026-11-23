// Port of mj-tui/src/wizards/projects.rs.

// Project discovery state and controls. All filesystem and network work is
// requested as dashboard actions; this module only edits the draft.

package tui

import (
	"fmt"
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
	panic("unported: mj-tui/src/wizards/projects.rs ProjectPicker::eq")
}

func (p *ProjectPicker) githubQueryNeedsSearch() bool {
	if p.tab != ProjectTabGithub {
		return false
	}
	request, ok := p.request.(core.ProjectDiscoveryRequestGithub)
	return !ok || request.Query != strings.TrimSpace(p.query.Value())
}

func (p *ProjectPicker) githubQueryChanged() {
	panic("unported: mj-tui/src/wizards/projects.rs ProjectPicker::github_query_changed")
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
	panic("unported: mj-tui/src/wizards/projects.rs NewWizard::choose_project_tab")
}

func (w *NewWizard) declareProjectControls(form *chat.Dialog[WizardControl]) {
	panic("unported: mj-tui/src/wizards/projects.rs NewWizard::declare_project_controls")
}

// ProjectCatalogContext identifies the open project picker's catalog
// request, when the new-session wizard is open.
func (d *DashboardState) ProjectCatalogContext() (string, bool) {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::project_catalog_context")
}

func (d *DashboardState) recentProjectEntries() []core.ProjectEntry {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::recent_project_entries")
}

func (d *DashboardState) ApplyProjectCatalogStatus(status core.ProjectCatalogStatus) {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::apply_project_catalog_status")
}

// TakeProjectDiscovery is the discovery the open project picker is waiting
// to have sent, if any.
func (d *DashboardState) TakeProjectDiscovery() DashboardAction {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::take_project_discovery")
}

// ApplyProjectDiscovery applies a discovery answer (Rust's
// `Result<ProjectDiscovery, String>`: err set for Err) to the picker that
// asked for it.
func (d *DashboardState) ApplyProjectDiscovery(context string, result core.ProjectDiscovery, err error) {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::apply_project_discovery")
}

func (d *DashboardState) activateProjectControl(wizard *NewWizard, id WizardControl) DashboardAction {
	panic("unported: mj-tui/src/wizards/projects.rs DashboardState::activate_project_control")
}

func renderProjectPicker(frame *chat.Frame, area uv.Rectangle, wizard *NewWizard, form *chat.Dialog[WizardControl], surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/wizards/projects.rs render_project_picker")
}
