// Port of mj-tui/src/wizards/dashboard/paths.rs.

// The shared path-completion popup: which path field of a screen it serves,
// and the replies that come back for it from background completion and
// validation.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CompletesPaths is a screen that owns path fields the shared completion
// popup can serve.
//
// Rust's dismiss_unfocused_completions has an empty default; Go interfaces
// have no defaults, so a screen without path fields to close declares an
// empty method.
type CompletesPaths interface {
	// focusedPathInput is the focused path field, the host that owns its
	// path, and what to list; ok is false when the focused control is not a
	// completable path.
	focusedPathInput(dashboard *DashboardState) (input *chat.PathInput, host core.CompletionHost, kind core.CompletionKind, ok bool)

	// dismissUnfocusedCompletions closes the popup of any path field the
	// keyboard has left. Only the focused field may keep one: Tab moves focus
	// before the form reports the dismissal, and a popup over a field nobody
	// is editing is noise.
	dismissUnfocusedCompletions()
}

// routePathCompletion consumes the completion interactions every screen
// shares, so no screen repeats the popup's key handling. consumed true is
// Rust's Ok(action); false hands the interaction back (rest, nil for Rust's
// None) for the screen's own match.
func routePathCompletion[K comparable](dashboard *DashboardState, screen CompletesPaths, interaction chat.Interaction[K]) (action DashboardAction, rest chat.Interaction[K], consumed bool) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs route_path_completion")
}

// focusedWizardPathInput is the focused path field of either wizard: the
// shared mount source, or a field the wizard alone has.
func focusedWizardPathInput(wizard WizardDraft, dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs focused_wizard_path_input")
}

// dismissUnfocusedWizardCompletions closes the popup of every wizard path
// field that is not focused.
func dismissUnfocusedWizardCompletions(wizard WizardDraft) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs dismiss_unfocused_wizard_completions")
}

func (w *NewWizard) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	return focusedWizardPathInput(w, dashboard)
}

func (w *NewWizard) dismissUnfocusedCompletions() {
	dismissUnfocusedWizardCompletions(w)
}

func (w *ResumeWizard) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	return focusedWizardPathInput(w, dashboard)
}

func (w *ResumeWizard) dismissUnfocusedCompletions() {
	dismissUnfocusedWizardCompletions(w)
}

// ApplyPathCompletions applies a completion reply to the field that asked
// for it. A reply for a draft the screen has moved on from is dropped: typed
// input always outranks a suggestion.
func (d *DashboardState) ApplyPathCompletions(context string, prefix string, completion core.PathCompletion) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::apply_path_completions")
}

// ApplyMountSourceValidation applies the host's answer about one mount
// source: forced is why the overlay cannot be used, err why the source was
// refused. A source whose filesystem cannot hold the overlay is remembered,
// so the entry is attached read-only and the editor locks the checkbox from
// then on.
func (d *DashboardState) ApplyMountSourceValidation(source string, forced *string, err error) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::apply_mount_source_validation")
}

// ApplyResolvedProjectDirectory applies the resolved project directory, its
// managed-worktree options and whether review turns off there, or err when
// it could not be resolved.
func (d *DashboardState) ApplyResolvedProjectDirectory(context string, directory string, resolved string, options core.ManagedWorktreeOptions, reviewTurnsOff bool, err error) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::apply_resolved_project_directory")
}

// ApplyResolvedMountSource applies the resolved mount source and why its
// overlay is forced off, or err when it could not be resolved.
func (d *DashboardState) ApplyResolvedMountSource(context string, source string, resolved string, forced *string, err error) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::apply_resolved_mount_source")
}

// PathInputContext identifies the draft a background path operation is
// allowed to update.
func (d *DashboardState) PathInputContext() string {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::path_input_context")
}

func (d *DashboardState) ApplyProjectDirectoryValidation(directory string, err error) {
	panic("unported: mj-tui/src/wizards/dashboard/paths.rs DashboardState::apply_project_directory_validation")
}

// applyFocusedCompletion applies a reply to whichever field of screen is
// focused, answering whether the field took it.
func applyFocusedCompletion(screen CompletesPaths, dashboard *DashboardState, prefix string, completion core.PathCompletion) bool {
	input, _, _, ok := screen.focusedPathInput(dashboard)
	if !ok {
		return false
	}
	return input.ApplyCompletion(prefix, completion)
}
