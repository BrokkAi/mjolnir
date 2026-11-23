// Port of mj-tui/src/wizards/dashboard/paths.rs.

// The shared path-completion popup: which path field of a screen it serves,
// and the replies that come back for it from background completion and
// validation.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"strings"

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
	screen.dismissUnfocusedCompletions()
	switch it := interaction.(type) {
	case chat.InteractionComplete[K]:
		input, host, kind, ok := screen.focusedPathInput(dashboard)
		if !ok {
			return DashboardActionNone{}, nil, true
		}
		if prefix, ok := input.RequestCompletion(); ok {
			return DashboardActionCompletePath{Host: host, Kind: kind, Prefix: prefix}, nil, true
		}
		return DashboardActionNone{}, nil, true
	case chat.InteractionPathCommit[K]:
		if input, _, _, ok := screen.focusedPathInput(dashboard); ok {
			input.SelectCompletion(it.Index)
			input.AcceptCompletion()
		}
		return DashboardActionNone{}, nil, true
	case chat.InteractionPathDismiss[K]:
		if input, _, _, ok := screen.focusedPathInput(dashboard); ok {
			input.DismissCompletion()
		}
		return DashboardActionNone{}, nil, true
	case chat.InteractionSelect[K]:
		// A list elsewhere on the screen still owns its own selection, so
		// only a field with an open popup takes this one.
		if input, _, _, ok := screen.focusedPathInput(dashboard); ok && input.IsCompleting() {
			input.SelectCompletion(it.Index)
			return DashboardActionNone{}, nil, true
		}
	}
	return nil, interaction, false
}

// focusedWizardPathInput is the focused path field of either wizard: the
// shared mount source, or a field the wizard alone has.
func focusedWizardPathInput(wizard WizardDraft, dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	if focused, ok := wizard.Form().Focused(); ok && focused == WizardControlMountSource {
		host := core.CompletionHostTarget{ID: nthKey(dashboard.config.Get().Targets, wizard.Target())}
		return &wizard.MountsMut().source, host, core.CompletionKindDirectories, true
	}
	input, host, kind := wizard.FocusedExtraPathInput(dashboard)
	if input == nil {
		return nil, nil, 0, false
	}
	return input, host, kind, true
}

// dismissUnfocusedWizardCompletions closes the popup of every wizard path
// field that is not focused.
func dismissUnfocusedWizardCompletions(wizard WizardDraft) {
	focused, ok := wizard.Form().Focused()
	if !ok || focused != WizardControlMountSource {
		wizard.MountsMut().source.DismissCompletion()
	}
	if ok {
		wizard.DismissUnfocusedExtraCompletions(&focused)
	} else {
		wizard.DismissUnfocusedExtraCompletions(nil)
	}
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
	if d.PathInputContext() != context {
		return
	}
	// The screen is detached so it can be handed its dashboard while the
	// dashboard's own mode is not borrowed through it.
	mode := d.mode
	d.mode = ModeDashboard{}
	switch screen := mode.(type) {
	case ModeNew:
		applyFocusedCompletion(screen.NewWizard, d, prefix, completion)
	case ModeResume:
		applyFocusedCompletion(screen.ResumeWizard, d, prefix, completion)
	case ModeEditContainer:
		if applyFocusedCompletion(screen.ContainerEditor, d, prefix, completion) {
			screen.prepare()
		}
	case ModeSetup:
		if applyFocusedCompletion(screen.SetupDialog, d, prefix, completion) {
			screen.prepare()
		}
	case ModeRepositoryOrigin:
		applyFocusedCompletion(screen.RepositoryOriginDialog, d, prefix, completion)
	}
	d.mode = mode
}

// ApplyMountSourceValidation applies the host's answer about one mount
// source: forced is why the overlay cannot be used, err why the source was
// refused. A source whose filesystem cannot hold the overlay is remembered,
// so the entry is attached read-only and the editor locks the checkbox from
// then on.
func (d *DashboardState) ApplyMountSourceValidation(source string, forced *string, err error) DashboardAction {
	enteredMoveReview := false
	enteredNewReview := false
	_, newSession := d.mode.(ModeNew)
	var mounts *MountWizard
	var form *chat.Dialog[WizardControl]
	var step *WizardStep
	moving := false
	switch wizard := d.mode.(type) {
	case ModeNew:
		if wizard.step == WizardStepMounts && wizard.mounts.source.Value() == source {
			mounts, form, step = &wizard.mounts, &wizard.form, &wizard.step
		}
	case ModeResume:
		if wizard.step == WizardStepMounts && wizard.mounts.source.Value() == source {
			mounts, form, step = &wizard.mounts, &wizard.form, &wizard.step
			moving = wizard.moving
		}
	}
	if mounts == nil {
		return DashboardActionNone{}
	}
	form.SetSubmissionPending(false)
	if err == nil {
		if forced != nil {
			if mounts.forcedSources == nil {
				mounts.forcedSources = make(map[string]string)
			}
			mounts.forcedSources[strings.TrimSpace(source)] = *forced
			mounts.forbidOverlay()
		}
		mounts.addValidatedMount()
		form.ForgetDraftPart("attachment editor")
		mounts.selectedMount = max(len(mounts.mounts)-1, 0)
		form.Focus(WizardControlReviewAttachments)
		*step = WizardStepReview
		enteredMoveReview = moving
		enteredNewReview = newSession
	} else {
		message := err.Error()
		mounts.error = &message
		form.Focus(WizardControlMountSource)
	}
	// A changed directory list needs a new prerequisite check, and a
	// corrected directory must not leave its earlier failure on the review.
	if enteredNewReview {
		mode, ok := d.mode.(ModeNew)
		if !ok {
			panic("new-session mount validation entered review without a new wizard")
		}
		d.mode = ModeDashboard{}
		d.invalidateNewRemotePreflight(mode.NewWizard)
		d.mode = mode
	}
	if enteredMoveReview {
		mode, ok := d.mode.(ModeResume)
		if !ok {
			panic("move mount validation entered review without a resume wizard")
		}
		d.mode = ModeDashboard{}
		profiles := d.compatibleProfiles(mode.sessionID)
		if mode.profile < 0 || mode.profile >= len(profiles) {
			panic("move wizard is only opened with a compatible profile")
		}
		return d.requestMovePreparationForReview(mode.ResumeWizard, profiles[mode.profile].ID)
	}
	return DashboardActionNone{}
}

// ApplyResolvedProjectDirectory applies the resolved project directory, its
// managed-worktree options and whether review turns off there, or err when
// it could not be resolved.
func (d *DashboardState) ApplyResolvedProjectDirectory(context string, directory string, resolved string, options core.ManagedWorktreeOptions, reviewTurnsOff bool, err error) {
	if d.PathInputContext() != context {
		return
	}
	if err != nil {
		if mode, ok := d.mode.(ModeNew); ok {
			mode.remotePreflightInFlight = false
			if mode.step == WizardStepReview || mode.step == WizardStepLaunching {
				message := err.Error()
				mode.remotePreflightError = &message
				mode.form.Focus(WizardControlSubmit)
			}
		}
		d.ApplyProjectDirectoryValidation(directory, err)
		return
	}
	value := resolved
	if mode, ok := d.mode.(ModeNew); ok {
		wizard := mode.NewWizard
		target := nthKey(d.config.Get().Targets, wizard.target)
		if previous := wizard.worktreeOptions; previous == nil ||
			previous.TargetID != target || previous.Directory != value {
			if d.goMode == nil {
				wizard.createManagedWorktree = options.DefaultCreate
			} else {
				recipe := d.goMode.Recipe
				wizard.createManagedWorktree = recipe != nil &&
					recipe.CreateManagedWorktree != nil && *recipe.CreateManagedWorktree
			}
		}
		if !options.Available {
			wizard.createManagedWorktree = false
		}
		wizard.worktreeOptions = &WorktreeDirectoryCheck{
			TargetID:       target,
			Directory:      value,
			Options:        options,
			ReviewTurnsOff: reviewTurnsOff,
		}
		wizard.remotePreflightInFlight = false
		wizard.remotePreflightError = nil
		wizard.projectDirectory.SetValue(value)
	}
	d.ApplyProjectDirectoryValidation(value, nil)
}

// ApplyResolvedMountSource applies the resolved mount source and why its
// overlay is forced off, or err when it could not be resolved.
func (d *DashboardState) ApplyResolvedMountSource(context string, source string, resolved string, forced *string, err error) DashboardAction {
	if d.PathInputContext() != context {
		return DashboardActionNone{}
	}
	if err != nil {
		return d.ApplyMountSourceValidation(source, nil, err)
	}
	value := resolved
	switch wizard := d.mode.(type) {
	case ModeNew:
		wizard.mounts.source.SetValue(value)
	case ModeResume:
		wizard.mounts.source.SetValue(value)
	default:
		return DashboardActionNone{}
	}
	return d.ApplyMountSourceValidation(value, forced, nil)
}

// PathInputContext identifies the draft a background path operation is
// allowed to update.
func (d *DashboardState) PathInputContext() string {
	config := d.config.Get()
	var draft string
	switch mode := d.mode.(type) {
	case ModeNew:
		w := mode.NewWizard
		draft = fmt.Sprintf(
			"new:%s:%d:%q:%q:%q:%q:%v:%s",
			pathContextNthTarget(config, w.target),
			w.step,
			w.projectDirectory.Value(),
			w.newBundleSource.Value(),
			w.mounts.source.Value(),
			w.mounts.destination.Value(),
			w.mounts.access,
			w.projectPicker.context(),
		)
	case ModeResume:
		w := mode.ResumeWizard
		draft = fmt.Sprintf(
			"resume:%s:%s:%d:%q:%q:%v",
			w.sessionID,
			pathContextNthTarget(config, w.target),
			w.step,
			w.mounts.source.Value(),
			w.mounts.destination.Value(),
			w.mounts.access,
		)
	case ModeSetup:
		draft = mode.pathInputContext()
	case ModeEditContainer:
		e := mode.ContainerEditor
		target := "None"
		if session := d.state.Get().Sessions.GetShared(e.sessionID); session != nil {
			if template, ok := config.Targets[session.TargetTemplateID]; ok {
				target = "Some(" + pathContextTemplate(template) + ")"
			}
		}
		draft = fmt.Sprintf(
			"container:%s:%s:%q:%q:%v",
			e.sessionID,
			target,
			e.source.Value(),
			e.destination.Value(),
			e.access,
		)
	case ModeRepositoryOrigin:
		draft = fmt.Sprintf(
			"origin:%s:%s:%s",
			mode.sessionID,
			mode.repositoryID,
			mode.replacement.Value(),
		)
	}
	return fmt.Sprintf("%d:%s", d.SessionPreflightGeneration(), draft)
}

func (d *DashboardState) ApplyProjectDirectoryValidation(directory string, err error) {
	mode, ok := d.mode.(ModeNew)
	if !ok {
		return
	}
	wizard := mode.NewWizard
	if (wizard.step != WizardStepProjectDirectory && wizard.step != WizardStepReview &&
		wizard.step != WizardStepLaunching) ||
		strings.TrimSpace(wizard.projectDirectory.Value()) != directory {
		return
	}
	wizard.form.SetSubmissionPending(false)
	if err == nil {
		wizard.projectDirectoryError = nil
		if wizard.SkipsReview() {
			wizard.step = WizardStepLaunching
		} else {
			wizard.step = WizardStepReview
		}
		wizard.form.Focus(WizardControlSubmit)
		return
	}
	message := err.Error()
	if wizard.projectDirectoryError == nil || *wizard.projectDirectoryError != message {
		wizard.projectDirectoryError = &message
	}
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

// pathContextNthTarget names the index-th configured target and its
// settings, in key order, for PathInputContext (Rust's Debug of
// `config.targets.iter().nth(index)`). The text is only compared with
// itself, so it needs to change exactly when the target does.
func pathContextNthTarget(config *core.Config, index int) string {
	ids := slices.Sorted(maps.Keys(config.Targets))
	if index < 0 || index >= len(ids) {
		return "None"
	}
	return fmt.Sprintf("Some((%q, %s))", ids[index], pathContextTemplate(config.Targets[ids[index]]))
}

// pathContextTemplate is a target template's settings as PathInputContext
// compares them: its wire form, which carries every field by value.
func pathContextTemplate(template core.TargetTemplate) string {
	encoded, err := json.Marshal(template)
	if err != nil {
		return fmt.Sprintf("unencodable target: %s", err)
	}
	return string(encoded)
}
